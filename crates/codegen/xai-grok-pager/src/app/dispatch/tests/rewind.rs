//! Tests for conversation rewind dispatchers and prompt-entry lookup.

use super::*;

#[test]
fn cancel_does_not_rewind_when_in_flight_block_committed() {
    // Minimal-mode regression: a user-prompt block commits to native
    // scrollback immediately (it is never `is_running`), and a committed
    // block can't be "un-printed". Cancelling must NOT rewind such a block —
    // doing so would `remove_entry` it from state while the printed copy
    // stays on screen AND restore the text into the input, showing the prompt
    // twice (dogfood bug: double-Esc on a just-promoted queued prompt).
    let mut app = test_app_with_agent();
    let id = AgentId(0);

    dispatch(Action::SendPrompt("queued prompt".into()), &mut app);
    assert!(app.agents[&id].session.in_flight_prompt.is_some());
    assert_eq!(app.agents[&id].scrollback.len(), 1);

    // Simulate minimal's commit pass printing the user block into native
    // scrollback (sets the entry's `committed` flag).
    let entry_id = app.agents[&id]
        .session
        .in_flight_prompt
        .as_ref()
        .unwrap()
        .scrollback_entry;
    let idx = app.agents[&id].scrollback.index_of_id(entry_id).unwrap();
    app.agents
        .get_mut(&id)
        .unwrap()
        .scrollback
        .mark_committed(idx);

    let effects = dispatch(Action::CancelTurn, &mut app);
    assert_eq!(effects.len(), 1);
    assert!(matches!(&effects[0], Effect::CancelTurn { .. }));

    // Standard cancel, NOT the rewind: the prompt is not restored to the
    // input and the committed block stays in scrollback (no duplicate).
    assert!(
        app.agents[&id].prompt.text().is_empty(),
        "committed in-flight block must not be rewound into the input"
    );
    assert_eq!(
        app.agents[&id].scrollback.len(),
        1,
        "committed block must stay in scrollback (it's already printed)"
    );
    assert!(app.agents[&id].session.state.is_cancelling());
}

#[test]
fn rewind_then_resubmit_drains_immediately_and_discards_orphan() {
    // After a rewind, state is Idle so a follow-up prompt can drain
    // without waiting for the cancelled turn's PromptResponse.
    let mut app = test_app_with_agent();
    let id = AgentId(0);

    dispatch(Action::SendPrompt("first".into()), &mut app);
    let first_pid = app.agents[&id].session.current_prompt_id.clone();
    assert!(first_pid.is_some());
    dispatch(Action::CancelTurn, &mut app);
    assert!(app.agents[&id].session.state.is_idle());
    assert!(app.agents[&id].session.current_prompt_id.is_none());

    // User edits and re-submits without waiting.
    let effects = dispatch(Action::SendPrompt("second".into()), &mut app);
    assert_eq!(effects.len(), 1);
    assert!(matches!(&effects[0], Effect::SendPrompt { text, .. } if text == "second"));
    assert!(app.agents[&id].session.state.is_turn_running());
    let second_pid = app.agents[&id].session.current_prompt_id.clone();
    assert!(second_pid.is_some());
    assert_ne!(first_pid, second_pid);

    // The cancelled "first" PromptResponse arrives mid-second-turn,
    // carrying first_pid. Mismatch with current_prompt_id (second_pid)
    // → discarded. State for "second" is untouched.
    dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::Cancelled).meta(
                serde_json::json!({ "promptId": first_pid })
                    .as_object()
                    .cloned(),
            )),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );
    assert!(app.agents[&id].session.state.is_turn_running());
    assert_eq!(app.agents[&id].session.current_prompt_id, second_pid);
}

/// Set up an app whose agent has one user prompt + one agent message in
/// the transcript, is inline-editing that prompt with `edited` typed in,
/// and has an unrelated draft sitting in the composer.
fn app_mid_inline_edit(edited: &str) -> AppView {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let agent = app.agents.get_mut(&id).unwrap();
    agent
        .scrollback
        .push_block(RenderBlock::user_prompt("fix the bug"));
    agent
        .scrollback
        .push_block(RenderBlock::agent_message("done"));
    agent.scrollback.prepare_layout(80, 40);
    assert!(agent.enter_inline_edit(0));
    agent
        .inline_edit
        .as_mut()
        .unwrap()
        .textarea
        .set_text(edited);
    agent.prompt.set_text("composer draft");
    app
}

/// Rewind point for the fixture's single prompt.
fn rewind_point(
    prompt_index: usize,
    has_file_changes: bool,
) -> crate::views::rewind::RewindPointInfo {
    crate::views::rewind::RewindPointInfo {
        prompt_index,
        created_at: String::new(),
        num_file_snapshots: 0,
        prompt_preview: Some("fix the bug".into()),
        has_file_changes,
    }
}

/// Points-loaded task result carrying the fixture's single rewind point.
fn points_loaded(id: AgentId, has_file_changes: bool) -> Action {
    Action::TaskComplete(TaskResult::RewindPointsLoaded {
        agent_id: id,
        points: vec![rewind_point(0, has_file_changes)],
    })
}

/// Successful rewind/execute response in the given mode.
fn rewind_success(
    target: usize,
    mode: &str,
    prompt_text: &str,
) -> crate::views::rewind::RewindResponse {
    crate::views::rewind::RewindResponse {
        success: true,
        target_prompt_index: target,
        reverted_files: vec![],
        clean_files: vec![],
        conflicts: vec![],
        error: None,
        mode: Some(mode.into()),
        prompt_text: Some(prompt_text.into()),
    }
}

/// Drive an idle inline-edit submit through the classic flow up to the
/// rewind execute: points fetch → ModeSelect → "conversation only" →
/// last-prompt confirm popup → Executing. Returns the effects of the step
/// that emitted `RewindExecute`.
fn drive_inline_submit_to_execute(app: &mut AppView) -> Vec<Effect> {
    let id = AgentId(0);
    let effects = dispatch(Action::InlineEditSubmit, app);
    assert!(
        matches!(&effects[0], Effect::FetchRewindPoints { .. }),
        "got {effects:?}"
    );
    dispatch(points_loaded(id, false), app);
    // Target 0 is the newest prompt: conversation-only inserts its
    // last-prompt confirm popup before executing, exactly like /rewind.
    dispatch(
        Action::RewindSelectMode(crate::views::rewind::RewindMode::ConversationOnly, 0),
        app,
    );
    dispatch(Action::RewindConversationOnlyConfirm(0), app)
}

/// Submitting an inline edit enters the exact same flow as `/rewind`: a
/// Loading overlay + a points fetch pre-targeted at the edited prompt. The
/// editor stays open behind the flow and nothing is stashed yet.
#[test]
fn inline_edit_submit_enters_rewind_flow_via_points_fetch() {
    let mut app = app_mid_inline_edit("fix the bug properly");
    let id = AgentId(0);

    let effects = dispatch(Action::InlineEditSubmit, &mut app);

    assert_eq!(effects.len(), 1);
    assert!(matches!(&effects[0], Effect::FetchRewindPoints { .. }));
    let agent = &app.agents[&id];
    let state = agent.rewind_state.as_ref().expect("rewind flow entered");
    assert!(matches!(
        state.phase,
        crate::views::rewind::RewindPhase::Loading
    ));
    assert_eq!(
        state.selected_prompt_index,
        Some(0),
        "pre-targeted at the edited prompt"
    );
    assert!(agent.inline_edit.is_some(), "editor stays open");
    assert!(
        agent.pending_inline_resubmit.is_none(),
        "nothing stashed before an execute"
    );
}

/// A submit whose text is unchanged (or empty) has nothing to do: the
/// editor just closes; no rewind flow, no effects.
#[test]
fn inline_edit_submit_with_unchanged_text_closes_editor() {
    let mut app = app_mid_inline_edit("fix the bug");
    let id = AgentId(0);

    let effects = dispatch(Action::InlineEditSubmit, &mut app);

    assert!(effects.is_empty());
    let agent = &app.agents[&id];
    assert!(agent.inline_edit.is_none(), "editor closed");
    assert!(agent.rewind_state.is_none(), "no rewind flow entered");
    assert!(agent.scrollback.inline_edit_height().is_none());
}

/// Points loaded with a pre-selected target skip the picker straight into
/// ModeSelect — the same popup `/rewind` shows — with the editor still open
/// behind it. In this inline context the "File changes only" row is hidden
/// (`offer_files_only: false`): the conversation rewind is a given.
#[test]
fn inline_edit_points_loaded_opens_mode_select_over_open_editor() {
    let mut app = app_mid_inline_edit("fix the bug properly");
    let id = AgentId(0);
    dispatch(Action::InlineEditSubmit, &mut app);

    dispatch(points_loaded(id, true), &mut app);

    let agent = &app.agents[&id];
    assert!(matches!(
        agent.rewind_state.as_ref().unwrap().phase,
        crate::views::rewind::RewindPhase::ModeSelect {
            target_prompt_index: 0,
            has_file_changes: true,
            offer_files_only: false,
            ..
        }
    ));
    assert!(agent.inline_edit.is_some(), "editor still open");
    assert!(agent.pending_inline_resubmit.is_none());
}

/// The classic `/rewind` flow is untouched: with no inline editor open, the
/// ModeSelect popup keeps all three options (files-only row offered).
#[test]
fn classic_rewind_mode_select_keeps_files_only_row() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent
            .scrollback
            .push_block(RenderBlock::user_prompt("fix the bug"));
        agent
            .scrollback
            .push_block(RenderBlock::agent_message("done"));
        agent.scrollback.prepare_layout(80, 40);
        agent.scrollback.set_selected(Some(0));
    }

    let effects = dispatch(Action::Rewind, &mut app);
    assert!(
        matches!(&effects[0], Effect::FetchRewindPoints { .. }),
        "got {effects:?}"
    );
    dispatch(points_loaded(id, true), &mut app);

    assert!(matches!(
        app.agents[&id].rewind_state.as_ref().unwrap().phase,
        crate::views::rewind::RewindPhase::ModeSelect {
            offer_files_only: true,
            ..
        }
    ));
}

/// Backspace from the preview Confirm during an inline flow returns to a
/// ModeSelect that still hides the files-only row (the editor is still
/// open, so the inline context is re-derived).
#[test]
fn inline_edit_back_to_mode_select_preserves_hidden_files_only_row() {
    let mut app = app_mid_inline_edit("fix the bug properly");
    let id = AgentId(0);
    dispatch(Action::InlineEditSubmit, &mut app);
    dispatch(points_loaded(id, true), &mut app);
    dispatch(
        Action::RewindSelectMode(crate::views::rewind::RewindMode::All, 0),
        &mut app,
    );
    let mut preview = rewind_success(0, "all", "fix the bug");
    preview.clean_files = vec!["src/main.rs".into()];
    dispatch(
        Action::TaskComplete(TaskResult::RewindPreviewComplete {
            agent_id: id,
            response: preview,
            target_prompt_index: 0,
            mode: crate::views::rewind::RewindMode::All,
        }),
        &mut app,
    );
    assert!(matches!(
        app.agents[&id].rewind_state.as_ref().unwrap().phase,
        crate::views::rewind::RewindPhase::Confirm { .. }
    ));

    dispatch(Action::RewindBackToModeSelect, &mut app);

    let agent = &app.agents[&id];
    assert!(matches!(
        agent.rewind_state.as_ref().unwrap().phase,
        crate::views::rewind::RewindPhase::ModeSelect {
            offer_files_only: false,
            ..
        }
    ));
    assert!(agent.inline_edit.is_some(), "editor still open");
}

/// ModeSelect → "conversation only" → execute: the edited text is stashed
/// exactly when the rewind executes; on success the transcript truncates
/// at the prompt, the edited text is resubmitted from there, the editor
/// closes, and the composer draft survives (no "Reverted conversation"
/// system note).
#[test]
fn inline_edit_conversation_only_success_resubmits_and_closes_editor() {
    let mut app = app_mid_inline_edit("fix the bug properly");
    let id = AgentId(0);
    dispatch(Action::InlineEditSubmit, &mut app);
    dispatch(points_loaded(id, false), &mut app);
    dispatch(
        Action::RewindSelectMode(crate::views::rewind::RewindMode::ConversationOnly, 0),
        &mut app,
    );
    // Newest prompt → the flow's last-prompt confirm popup shows first.
    assert!(matches!(
        app.agents[&id].rewind_state.as_ref().unwrap().phase,
        crate::views::rewind::RewindPhase::ConversationOnlyConfirm { .. }
    ));

    let effects = dispatch(Action::RewindConversationOnlyConfirm(0), &mut app);
    assert!(matches!(
        &effects[0],
        Effect::RewindExecute {
            target_prompt_index: 0,
            mode: crate::views::rewind::RewindMode::ConversationOnly,
            ..
        }
    ));
    {
        let agent = &app.agents[&id];
        assert_eq!(
            agent.pending_inline_resubmit.as_deref(),
            Some("fix the bug properly"),
            "stash armed at execute time"
        );
        assert!(
            agent.inline_edit.is_some(),
            "editor open until the rewind lands"
        );
    }

    let effects = dispatch(
        Action::TaskComplete(TaskResult::RewindExecuteComplete {
            agent_id: id,
            response: rewind_success(0, "conversation_only", "fix the bug"),
        }),
        &mut app,
    );

    assert!(
        effects.iter().any(
            |e| matches!(e, Effect::SendPrompt { text, .. } if text == "fix the bug properly")
        ),
        "edited prompt must be sent, got {effects:?}"
    );
    let agent = &app.agents[&id];
    assert!(agent.inline_edit.is_none(), "editor closed on success");
    assert!(agent.scrollback.inline_edit_height().is_none());
    assert!(agent.pending_inline_resubmit.is_none());
    assert_eq!(
        agent.prompt.text(),
        "composer draft",
        "composer draft survives"
    );
    // Transcript truncated at the prompt; only the resubmitted prompt block
    // remains (no "Reverted conversation" system note).
    assert_eq!(agent.scrollback.len(), 1);
    match &agent.scrollback.entry(0).unwrap().block {
        RenderBlock::UserPrompt(b) => assert_eq!(b.text, "fix the bug properly"),
        other => panic!("expected resubmitted user prompt, got {other:?}"),
    }
}

/// ModeSelect → "conversation + files" over a point with file changes goes
/// through the preview → Confirm popup (same as /rewind); confirming
/// executes with the edited text stashed, and success resubmits it.
#[test]
fn inline_edit_all_mode_previews_confirms_and_resubmits() {
    let mut app = app_mid_inline_edit("fix the bug properly");
    let id = AgentId(0);
    dispatch(Action::InlineEditSubmit, &mut app);
    dispatch(points_loaded(id, true), &mut app);

    let effects = dispatch(
        Action::RewindSelectMode(crate::views::rewind::RewindMode::All, 0),
        &mut app,
    );
    assert!(
        matches!(&effects[0], Effect::RewindPreview { .. }),
        "file changes present → preview first, got {effects:?}"
    );
    assert!(
        app.agents[&id].pending_inline_resubmit.is_none(),
        "no stash before an execute"
    );

    // Preview lands → Confirm popup, editor still open behind it.
    let mut preview = rewind_success(0, "all", "fix the bug");
    preview.clean_files = vec!["src/main.rs".into()];
    dispatch(
        Action::TaskComplete(TaskResult::RewindPreviewComplete {
            agent_id: id,
            response: preview,
            target_prompt_index: 0,
            mode: crate::views::rewind::RewindMode::All,
        }),
        &mut app,
    );
    assert!(matches!(
        app.agents[&id].rewind_state.as_ref().unwrap().phase,
        crate::views::rewind::RewindPhase::Confirm { .. }
    ));
    assert!(app.agents[&id].inline_edit.is_some());

    let effects = dispatch(
        Action::RewindConfirm(0, crate::views::rewind::RewindMode::All),
        &mut app,
    );
    assert!(matches!(
        &effects[0],
        Effect::RewindExecute {
            mode: crate::views::rewind::RewindMode::All,
            ..
        }
    ));
    assert_eq!(
        app.agents[&id].pending_inline_resubmit.as_deref(),
        Some("fix the bug properly")
    );

    let effects = dispatch(
        Action::TaskComplete(TaskResult::RewindExecuteComplete {
            agent_id: id,
            response: rewind_success(0, "all", "fix the bug"),
        }),
        &mut app,
    );
    assert!(
        effects.iter().any(
            |e| matches!(e, Effect::SendPrompt { text, .. } if text == "fix the bug properly")
        ),
        "got {effects:?}"
    );
    assert!(
        app.agents[&id].inline_edit.is_none(),
        "editor closed on success"
    );
}

/// ModeSelect → "files only": no conversation rewind happens, so nothing
/// is resubmitted — the edited text lands in the composer instead (never
/// silently dropped), the editor closes, and the conversation transcript
/// stays intact.
#[test]
fn inline_edit_files_only_success_prefills_composer_without_resubmit() {
    let mut app = app_mid_inline_edit("fix the bug properly");
    let id = AgentId(0);
    dispatch(Action::InlineEditSubmit, &mut app);
    // No file changes recorded → FilesOnly skips the preview and executes
    // directly.
    dispatch(points_loaded(id, false), &mut app);
    let effects = dispatch(
        Action::RewindSelectMode(crate::views::rewind::RewindMode::FilesOnly, 0),
        &mut app,
    );
    assert!(matches!(
        &effects[0],
        Effect::RewindExecute {
            mode: crate::views::rewind::RewindMode::FilesOnly,
            ..
        }
    ));
    assert_eq!(
        app.agents[&id].pending_inline_resubmit.as_deref(),
        Some("fix the bug properly")
    );

    let effects = dispatch(
        Action::TaskComplete(TaskResult::RewindExecuteComplete {
            agent_id: id,
            response: rewind_success(0, "files_only", "fix the bug"),
        }),
        &mut app,
    );

    assert!(
        effects
            .iter()
            .all(|e| !matches!(e, Effect::SendPrompt { .. })),
        "files-only must not resubmit, got {effects:?}"
    );
    let agent = &app.agents[&id];
    assert!(agent.inline_edit.is_none(), "editor closed");
    assert!(agent.pending_inline_resubmit.is_none());
    assert_eq!(
        agent.prompt.text(),
        "fix the bug properly",
        "edit surfaced in the composer"
    );
    // Conversation untouched: the original prompt + reply are still there
    // (the classic flow appends its "Reverted file changes" note after).
    assert!(matches!(
        &agent.scrollback.entry(0).unwrap().block,
        RenderBlock::UserPrompt(b) if b.text == "fix the bug"
    ));
    assert!(matches!(
        &agent.scrollback.entry(1).unwrap().block,
        RenderBlock::AgentMessage(_)
    ));
}

/// Dismissing the ModeSelect popup aborts the whole thing: the overlay
/// closes, nothing was stashed, and the editor is still open with the
/// edit intact.
#[test]
fn inline_edit_dismiss_from_mode_select_returns_to_editor() {
    let mut app = app_mid_inline_edit("fix the bug properly");
    let id = AgentId(0);
    dispatch(Action::InlineEditSubmit, &mut app);
    dispatch(points_loaded(id, false), &mut app);

    dispatch(Action::RewindDismiss, &mut app);

    let agent = &app.agents[&id];
    assert!(agent.rewind_state.is_none());
    assert!(agent.pending_inline_resubmit.is_none());
    assert_eq!(
        agent
            .inline_edit
            .as_ref()
            .expect("editor still open")
            .textarea
            .text(),
        "fix the bug properly"
    );
}

/// Submitting mid-turn raises the same cancel-offer `/rewind` does,
/// pre-targeted at the edited prompt, over the still-open editor;
/// confirming cancels the turn and re-enters the flow via a points fetch.
#[test]
fn inline_edit_busy_submit_cancel_offer_confirm_cancels_and_fetches_points() {
    let mut app = app_mid_inline_edit("fix the bug properly");
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().session.state = crate::app::agent::AgentState::TurnRunning;

    let effects = dispatch(Action::InlineEditSubmit, &mut app);
    assert!(effects.is_empty(), "no effects yet: {effects:?}");
    {
        let agent = &app.agents[&id];
        let state = agent.rewind_state.as_ref().unwrap();
        assert!(matches!(
            state.phase,
            crate::views::rewind::RewindPhase::CancelOffer { .. }
        ));
        assert_eq!(state.selected_prompt_index, Some(0));
        assert!(agent.inline_edit.is_some(), "editor open behind the offer");
        assert!(agent.pending_inline_resubmit.is_none());
    }

    let effects = dispatch(Action::RewindCancelOffer, &mut app);
    assert!(matches!(&effects[0], Effect::CancelTurn { .. }));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::FetchRewindPoints { .. })),
        "got {effects:?}"
    );
    let agent = &app.agents[&id];
    let state = agent.rewind_state.as_ref().unwrap();
    assert!(matches!(
        state.phase,
        crate::views::rewind::RewindPhase::Loading
    ));
    assert_eq!(
        state.selected_prompt_index,
        Some(0),
        "target survives the cancel"
    );
    assert!(agent.inline_edit.is_some(), "editor still open");
}

/// Dismissing the mid-turn cancel-offer ("let it finish") returns straight
/// to the still-open editor with the edit intact.
#[test]
fn inline_edit_busy_cancel_offer_dismiss_returns_to_editor() {
    let mut app = app_mid_inline_edit("fix the bug properly");
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().session.state = crate::app::agent::AgentState::TurnRunning;
    dispatch(Action::InlineEditSubmit, &mut app);

    dispatch(Action::RewindDismiss, &mut app);

    let agent = &app.agents[&id];
    assert!(agent.rewind_state.is_none());
    assert_eq!(
        agent
            .inline_edit
            .as_ref()
            .expect("editor open")
            .textarea
            .text(),
        "fix the bug properly"
    );
}

/// A failed execute drops the stashed resubmit but leaves the editor open
/// behind the error overlay — dismissing the error returns to editing.
#[test]
fn inline_edit_execute_failure_keeps_editor_open() {
    let mut app = app_mid_inline_edit("fix the bug properly");
    let id = AgentId(0);
    drive_inline_submit_to_execute(&mut app);
    assert!(app.agents[&id].pending_inline_resubmit.is_some());

    let effects = dispatch(
        Action::TaskComplete(TaskResult::RewindExecuteFailed {
            agent_id: id,
            error: "boom".into(),
        }),
        &mut app,
    );

    assert!(effects.is_empty());
    let agent = &app.agents[&id];
    assert!(
        agent.pending_inline_resubmit.is_none(),
        "stash dies with its rewind"
    );
    assert!(matches!(
        agent.rewind_state.as_ref().unwrap().phase,
        crate::views::rewind::RewindPhase::Error { .. }
    ));
    assert_eq!(
        agent
            .inline_edit
            .as_ref()
            .expect("editor open")
            .textarea
            .text(),
        "fix the bug properly"
    );
    assert_eq!(agent.scrollback.len(), 2, "transcript untouched");
}

/// A rewind/execute response with `success: false` likewise drops the
/// stash, shows the error overlay, and leaves the editor open.
#[test]
fn inline_edit_unsuccessful_response_keeps_editor_open() {
    let mut app = app_mid_inline_edit("fix the bug properly");
    let id = AgentId(0);
    drive_inline_submit_to_execute(&mut app);

    let mut response = rewind_success(0, "conversation_only", "fix the bug");
    response.success = false;
    response.error = Some("conflict".into());
    let effects = dispatch(
        Action::TaskComplete(TaskResult::RewindExecuteComplete {
            agent_id: id,
            response,
        }),
        &mut app,
    );

    assert!(effects.is_empty());
    let agent = &app.agents[&id];
    assert!(agent.pending_inline_resubmit.is_none());
    assert!(matches!(
        agent.rewind_state.as_ref().unwrap().phase,
        crate::views::rewind::RewindPhase::Error { .. }
    ));
    assert!(agent.inline_edit.is_some(), "editor stays open");
    assert_eq!(agent.scrollback.len(), 2, "transcript untouched");
}

/// An edited prompt that happens to start with "/" is resubmitted verbatim
/// as a prompt (literal), not executed as a slash command on the
/// already-truncated transcript.
#[test]
fn inline_edit_resubmit_sends_slash_text_literally() {
    let mut app = app_mid_inline_edit("/etc/hosts is wrong, fix it");
    let id = AgentId(0);
    drive_inline_submit_to_execute(&mut app);

    let effects = dispatch(
        Action::TaskComplete(TaskResult::RewindExecuteComplete {
            agent_id: id,
            response: rewind_success(0, "conversation_only", "fix the bug"),
        }),
        &mut app,
    );

    assert!(
        effects.iter().any(
            |e| matches!(e, Effect::SendPrompt { text, .. } if text == "/etc/hosts is wrong, fix it")
        ),
        "slash-lookalike edit must be sent as a prompt, got {effects:?}"
    );
}

/// If the user switches views while the rewind is in flight, the edited
/// text falls back into that agent's composer without clobbering an
/// existing draft (it is appended on a new line).
#[test]
fn inline_edit_rewind_success_after_view_switch_appends_to_draft() {
    let mut app = app_mid_inline_edit("fix the bug properly");
    let id = AgentId(0);
    drive_inline_submit_to_execute(&mut app);
    app.active_view = ActiveView::Welcome;

    let effects = dispatch(
        Action::TaskComplete(TaskResult::RewindExecuteComplete {
            agent_id: id,
            response: rewind_success(0, "conversation_only", "fix the bug"),
        }),
        &mut app,
    );

    assert!(
        effects
            .iter()
            .all(|e| !matches!(e, Effect::SendPrompt { .. })),
        "no resubmit while the view is elsewhere, got {effects:?}"
    );
    let agent = &app.agents[&id];
    assert_eq!(
        agent.prompt.text(),
        "composer draft\nfix the bug properly",
        "draft preserved, edited text appended"
    );
    assert!(agent.inline_edit.is_none(), "editor closed on success");
}

#[test]
fn inline_edit_view_switch_preserves_image_draft_when_appending_resubmit() {
    let mut app = app_mid_inline_edit("fix the bug properly");
    let id = AgentId(0);
    let draft_text = {
        let agent = app.agents.get_mut(&id).unwrap();
        let end = agent.prompt.text().len();
        agent.prompt.set_cursor(end);
        agent
            .prompt
            .insert_image(crate::prompt_images::from_clipboard_data(
                &crate::clipboard::ImageData {
                    data: vec![1, 2, 3],
                    mime_type: "image/png".into(),
                },
            ))
            .unwrap();
        agent.prompt.text().to_owned()
    };
    drive_inline_submit_to_execute(&mut app);
    app.active_view = ActiveView::Welcome;

    let effects = dispatch(
        Action::TaskComplete(TaskResult::RewindExecuteComplete {
            agent_id: id,
            response: rewind_success(0, "conversation_only", "fix the bug"),
        }),
        &mut app,
    );

    assert!(
        effects
            .iter()
            .all(|effect| !matches!(effect, Effect::SendPrompt { .. }))
    );
    let agent = app.agents.get_mut(&id).unwrap();
    assert_eq!(
        agent.prompt.text(),
        format!("{draft_text}\nfix the bug properly")
    );
    let restored_image_ids = agent
        .prompt
        .textarea
        .elements()
        .iter()
        .filter(|element| element.kind == crate::views::prompt_widget::KIND_IMAGE)
        .map(|element| element.id)
        .collect::<Vec<_>>();
    assert_eq!(
        restored_image_ids.len(),
        1,
        "restored draft must retain one image chip",
    );
    let images = agent.prompt.drain_images();
    assert_eq!(images.len(), 1, "reconciliation must retain the image");
    assert_eq!(
        images[0].element_id, restored_image_ids[0],
        "restored image must bind to the re-registered chip element",
    );
}

#[test]
fn stacked_rewinds_each_get_their_own_pid_and_orphans_drop_independently() {
    // Two rewinds → two cancelled PRs to drain. Each carries its own
    // promptId; both fail to match current_prompt_id (None) and are
    // silently discarded with no banner.
    let mut app = test_app_with_agent();
    let id = AgentId(0);

    dispatch(Action::SendPrompt("a".into()), &mut app);
    let pid_a = app.agents[&id].session.current_prompt_id.clone();
    dispatch(Action::CancelTurn, &mut app);
    dispatch(Action::SendPrompt("b".into()), &mut app);
    let pid_b = app.agents[&id].session.current_prompt_id.clone();
    dispatch(Action::CancelTurn, &mut app);
    assert_ne!(pid_a, pid_b);
    assert!(app.agents[&id].session.current_prompt_id.is_none());

    let pr = |pid: &Option<String>| {
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::Cancelled)
                .meta(serde_json::json!({ "promptId": pid }).as_object().cloned())),
            http_status: None,
            prompt_id: None,
        })
    };
    dispatch(pr(&pid_a), &mut app);
    dispatch(pr(&pid_b), &mut app);
    assert_eq!(app.agents[&id].scrollback.len(), 0);
    assert!(app.agents[&id].session.state.is_idle());
}

fn user_block(text: &str, pi: Option<usize>) -> RenderBlock {
    let mut b = UserPromptBlock::new(text);
    b.prompt_index = pi;
    RenderBlock::UserPrompt(b)
}

/// A successful conversation rewind truncates the transcript tail
/// (`remove_from`) — the purge must fire exactly once, and a
/// files-only rewind (no truncation) must not purge.
#[test]
fn rewind_success_truncation_releases_retained_memory() {
    use crate::memory_release::test_support;
    test_support::install_counting_hook();

    let response = |mode: &str| crate::views::rewind::RewindResponse {
        success: true,
        target_prompt_index: 0,
        reverted_files: Vec::new(),
        clean_files: Vec::new(),
        conflicts: Vec::new(),
        error: None,
        mode: Some(mode.into()),
        prompt_text: Some("alpha".into()),
    };

    // files_only: transcript untouched → no purge.
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    if let Some(agent) = app.agents.get_mut(&id) {
        agent.scrollback.push_block(user_block("alpha", Some(0)));
        agent.scrollback.push_block(RenderBlock::agent_message("a"));
    }
    let before = test_support::calls();
    dispatch(
        Action::TaskComplete(TaskResult::RewindExecuteComplete {
            agent_id: id,
            response: response("files_only"),
        }),
        &mut app,
    );
    assert_eq!(
        test_support::calls(),
        before,
        "a files-only rewind truncates nothing and must not purge"
    );

    // Conversation rewind: the tail from the anchor drops → one purge.
    let len_before = app.agents[&id].scrollback.len();
    let before = test_support::calls();
    dispatch(
        Action::TaskComplete(TaskResult::RewindExecuteComplete {
            agent_id: id,
            response: response("all"),
        }),
        &mut app,
    );
    assert!(
        app.agents[&id].scrollback.len() < len_before,
        "fixture sanity: the conversation rewind must truncate entries"
    );
    assert_eq!(
        test_support::calls(),
        before + 1,
        "the rewound tail dropped — exactly one purge"
    );
}

/// A successful rewind confirms via a toast in the full TUI; minimal mode
/// keeps the scrollback system block (it never renders toasts).
#[test]
fn rewind_success_toasts_in_full_tui_and_commits_system_block_in_minimal() {
    let response = |mode: &str| crate::views::rewind::RewindResponse {
        success: true,
        target_prompt_index: 0,
        reverted_files: Vec::new(),
        clean_files: Vec::new(),
        conflicts: Vec::new(),
        error: None,
        mode: Some(mode.into()),
        prompt_text: None,
    };

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    if let Some(agent) = app.agents.get_mut(&id) {
        agent.scrollback.push_block(user_block("alpha", Some(0)));
        agent.scrollback.push_block(RenderBlock::agent_message("a"));
    }
    dispatch(
        Action::TaskComplete(TaskResult::RewindExecuteComplete {
            agent_id: id,
            response: response("all"),
        }),
        &mut app,
    );
    assert_eq!(
        app.agents[&id].toast.as_ref().map(|(m, _)| m.as_str()),
        Some("Reverted conversation and file changes")
    );
    assert_eq!(
        app.agents[&id].scrollback.len(),
        0,
        "the confirmation must not land in scrollback in the full TUI"
    );

    let mut app = test_app_with_agent();
    app.screen_mode = crate::app::ScreenMode::Minimal;
    dispatch(
        Action::TaskComplete(TaskResult::RewindExecuteComplete {
            agent_id: id,
            response: response("files_only"),
        }),
        &mut app,
    );
    assert!(app.agents[&id].toast.is_none());
    assert_eq!(last_system_text(&app, id), "Reverted file changes");
}

#[test]
fn primary_path_returns_correct_idx_for_each_prompt() {
    let mut sb = ScrollbackState::new();
    let alpha = sb.push_block(user_block("alpha", Some(0)));
    sb.push_block(RenderBlock::agent_message("a"));
    let bravo = sb.push_block(user_block("bravo", Some(1)));
    sb.push_block(RenderBlock::agent_message("b"));
    let charlie = sb.push_block(user_block("charlie", Some(2)));
    sb.push_block(RenderBlock::agent_message("c"));

    let alpha_idx = sb.index_of_id(alpha).unwrap();
    let bravo_idx = sb.index_of_id(bravo).unwrap();
    let charlie_idx = sb.index_of_id(charlie).unwrap();

    assert_eq!(
        find_user_prompt_entry_for_shell_index(&sb, 0),
        Some(alpha_idx)
    );
    assert_eq!(
        find_user_prompt_entry_for_shell_index(&sb, 1),
        Some(bravo_idx)
    );
    assert_eq!(
        find_user_prompt_entry_for_shell_index(&sb, 2),
        Some(charlie_idx)
    );
}

/// Interjections render as standard user prompts but the shell never
/// numbers them — the positional fallback must skip them or every mapping
/// after an interjection is off by one.
#[test]
fn fallback_path_skips_interjections() {
    let mut sb = ScrollbackState::new();
    let alpha = sb.push_block(user_block("alpha", None));
    sb.push_block(RenderBlock::agent_message("a"));
    sb.push_block(RenderBlock::interjection_prompt("mid-turn steer"));
    sb.push_block(RenderBlock::agent_message("a2"));
    let bravo = sb.push_block(user_block("bravo", None));

    let alpha_idx = sb.index_of_id(alpha).unwrap();
    let bravo_idx = sb.index_of_id(bravo).unwrap();

    assert_eq!(
        find_user_prompt_entry_for_shell_index(&sb, 0),
        Some(alpha_idx)
    );
    assert_eq!(
        find_user_prompt_entry_for_shell_index(&sb, 1),
        Some(bravo_idx),
        "index 1 must map to the next real prompt, not the interjection"
    );
}

/// Selecting an interjection (or an entry after it within the same turn)
/// anchors rewind on the enclosing turn's prompt, not the next turn's.
#[test]
fn shell_prompt_index_at_resolves_interjection_to_enclosing_turn() {
    use super::super::rewind::shell_prompt_index_at;

    let mut sb = ScrollbackState::new();
    sb.push_block(user_block("alpha", Some(0)));
    sb.push_block(RenderBlock::agent_message("a"));
    let ij = sb.push_block(RenderBlock::interjection_prompt("mid-turn steer"));
    sb.push_block(RenderBlock::agent_message("a2"));
    sb.push_block(user_block("bravo", Some(1)));

    let ij_idx = sb.index_of_id(ij).unwrap();
    assert_eq!(shell_prompt_index_at(&sb, ij_idx), Some(0));
    // A block after the interjection but before the next prompt still
    // belongs to turn 0.
    assert_eq!(shell_prompt_index_at(&sb, ij_idx + 1), Some(0));
}

/// Legacy meta-less scrollbacks: the positional count inside
/// `shell_prompt_index_at` must also exclude interjections.
#[test]
fn shell_prompt_index_at_counting_fallback_skips_interjections() {
    use super::super::rewind::shell_prompt_index_at;

    let mut sb = ScrollbackState::new();
    sb.push_block(user_block("alpha", None));
    sb.push_block(RenderBlock::interjection_prompt("steer"));
    let bravo = sb.push_block(user_block("bravo", None));

    let bravo_idx = sb.index_of_id(bravo).unwrap();
    assert_eq!(shell_prompt_index_at(&sb, bravo_idx), Some(1));
}

#[test]
fn fallback_path_returns_correct_idx_when_prompt_index_is_none() {
    let mut sb = ScrollbackState::new();
    let alpha = sb.push_block(user_block("alpha", None));
    sb.push_block(RenderBlock::agent_message("a"));
    let bravo = sb.push_block(user_block("bravo", None));
    sb.push_block(RenderBlock::agent_message("b"));
    let charlie = sb.push_block(user_block("charlie", None));
    sb.push_block(RenderBlock::agent_message("c"));

    let alpha_idx = sb.index_of_id(alpha).unwrap();
    let bravo_idx = sb.index_of_id(bravo).unwrap();
    let charlie_idx = sb.index_of_id(charlie).unwrap();

    assert_eq!(
        find_user_prompt_entry_for_shell_index(&sb, 0),
        Some(alpha_idx)
    );
    assert_eq!(
        find_user_prompt_entry_for_shell_index(&sb, 1),
        Some(bravo_idx)
    );
    assert_eq!(
        find_user_prompt_entry_for_shell_index(&sb, 2),
        Some(charlie_idx)
    );
}
