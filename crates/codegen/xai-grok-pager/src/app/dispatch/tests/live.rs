//! Codex Live delegation-submit dispatch tests.
//!
//! Exercises `dispatch_live_delegation_submit` (via `Action::LiveDelegationSubmit`)
//! against a real `AppView` with a bound Live session. Covers:
//! - composer image isolation: draft images + a running turn must NOT divert
//!   the literal delegation into the local enqueue path (no `Effect::SendPrompt`
//!   → reported failure + untracked later work); the delegation takes the
//!   immediate-server-send path and the composer images are preserved.
//! - no-effect/no-residual: when the prompt is not accepted, no delegation is
//!   registered and no untracked queue entry remains.
//! - the centralized failure rail (`on_prompt_failed`) enqueues exactly one
//!   `CompleteDelegation` with the wrapped failure text and marks the registry
//!   terminal, replacing the duplicated dead-code rails.

#![cfg(feature = "codex-live")]

use super::*;
use crate::live::broker::LiveDelegationBroker;
use crate::live::state::{DraftSnapshot, LiveState};
use crate::live::{LiveCommand, LiveEvent, LiveRole, TranscriptKind};

/// Build a `PastedImage` from a minimal in-memory PNG (no disk I/O) large
/// enough to pass `insert_image`'s 8×8 minimum-side check. The delegation path
/// only inspects `prompt.images.is_empty()`, so a single placeholder image is
/// enough to reproduce the draft-images-while-running condition.
fn draft_image() -> crate::prompt_images::PastedImage {
    use image::{ImageBuffer, Rgba};
    let img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_pixel(8, 8, Rgba([0, 0, 0, 255]));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    crate::prompt_images::from_clipboard_data(&crate::clipboard::ImageData {
        data: bytes.into_inner(),
        mime_type: "image/png".to_string(),
    })
}

/// Set up an app with a single agent (AgentId(0)) on an agent view, a bound
/// session, and an ACTIVE Live session bound to that agent/session/generation.
/// The broker is bound and a `cmd_tx` channel is installed so `live_send_cmd`
/// can be observed.
fn app_with_active_live() -> (AppView, AgentId, tokio::sync::mpsc::Receiver<LiveCommand>) {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.live_mode_enabled = true;

    // Bound session already present from `test_app_with_agent` ("test-session").
    let session_id = app.agents[&id]
        .session
        .session_id
        .as_ref()
        .map(|s| s.0.as_ref().to_string())
        .expect("test agent has a bound session");

    // Install a Live command channel and bind the broker.
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<LiveCommand>(32);
    let generation = app.live_runtime.next_generation();
    let mut broker = LiveDelegationBroker::new(generation);
    broker.bind(session_id.clone(), id);
    app.live_runtime.broker = broker;
    app.live_runtime.cmd_tx = Some(cmd_tx);
    app.live_runtime.state = LiveState::Active {
        agent_id: id,
        session_id,
        generation,
        draft: DraftSnapshot::default(),
    };
    // Ensure the active view is the agent (the delegation validates this).
    app.active_view = ActiveView::Agent(id);
    (app, id, cmd_rx)
}

#[test]
fn delegation_with_draft_images_and_running_turn_sends_and_preserves_images() {
    let (mut app, id, _cmd_rx) = app_with_active_live();
    let generation = app.live_runtime.state.generation().unwrap();

    // Draft images attached to the composer + a running turn. Before the fix,
    // `immediate_server_send` was false (`images.is_empty()` gated it), so the
    // literal delegation was enqueued locally, no `Effect::SendPrompt` was
    // returned, and the delegation was reported as failed. Insert via the real
    // `insert_image` path so the image chip is bound to a textarea element
    // (drain_images reconciles against live elements).
    app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
    app.agents
        .get_mut(&id)
        .unwrap()
        .prompt
        .insert_image(draft_image())
        .expect("test image inserts");
    assert!(
        !app.agents[&id].prompt.images.is_empty(),
        "precondition: composer has draft images"
    );

    // Snapshot the composer draft (text + cursor) AFTER inserting the image,
    // exactly as `handle_delegation` does before submitting. The chip text
    // makes this non-empty, so `restore_draft`'s `set_text` does NOT drain
    // images — mirroring real usage where a composer with images is never
    // empty.
    let current_draft = DraftSnapshot {
        text: app.agents[&id].prompt.text().to_string(),
        cursor: app.agents[&id].prompt.cursor(),
    };
    assert!(
        !current_draft.text.is_empty(),
        "precondition: image chip makes the composer text non-empty"
    );

    let effects = dispatch(
        Action::LiveDelegationSubmit {
            agent_id: id,
            text: "do the thing".into(),
            delegation_id: "del-1".into(),
            generation,
            draft: current_draft,
        },
        &mut app,
    );

    // The delegation MUST take the immediate-server-send path and return an
    // `Effect::SendPrompt` matching the bound agent + session.
    let send = effects.iter().find_map(|e| match e {
        Effect::SendPrompt {
            prompt_id,
            text,
            agent_id: eff_agent,
            session_id: eff_session,
            ..
        } if *eff_agent == id
            && eff_session.0.as_ref() == "test-session"
            && text == "do the thing" =>
        {
            Some(prompt_id.clone())
        }
        _ => None,
    });
    let prompt_id = send.expect(
        "delegation with draft images + running turn must still produce a matching Effect::SendPrompt",
    );

    // The delegation is registered with the exact correlation.
    assert!(app.live_runtime.has_delegation(generation, "del-1"));
    assert!(
        !app.live_runtime.is_delegation_terminal(generation, "del-1"),
        "an accepted delegation must not be terminal"
    );

    // The composer images MUST be preserved (isolated around the dispatch).
    assert!(
        !app.agents[&id].prompt.images.is_empty(),
        "composer draft images must be restored after the delegation dispatch"
    );

    // No untracked local queue entry remains for the delegation text.
    let leaked = app.agents[&id]
        .session
        .pending_prompts
        .iter()
        .any(|p| p.text == "do the thing");
    assert!(
        !leaked,
        "no untracked local queue entry must remain for the delegation text"
    );

    // The prompt_id is non-empty and registered.
    assert!(!prompt_id.is_empty());
}

#[test]
fn delegation_preserves_preexisting_user_queue_and_rejects_without_dispatch() {
    let (mut app, id, mut cmd_rx) = app_with_active_live();
    let generation = app.live_runtime.state.generation().unwrap();

    let existing_id = app
        .agents
        .get_mut(&id)
        .unwrap()
        .session
        .enqueue_prompt("user queued work".to_string());
    app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;

    let effects = dispatch(
        Action::LiveDelegationSubmit {
            agent_id: id,
            text: "live delegated work".into(),
            delegation_id: "del-queued".into(),
            generation,
            draft: DraftSnapshot::default(),
        },
        &mut app,
    );

    assert!(
        effects.is_empty(),
        "delegation must not drain existing user work"
    );
    let pending = &app.agents[&id].session.pending_prompts;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, existing_id);
    assert_eq!(pending[0].text, "user queued work");
    assert!(!app.live_runtime.has_delegation(generation, "del-queued"));

    let cmd = cmd_rx
        .try_recv()
        .expect("rejection must complete the delegation");
    assert!(matches!(
        cmd,
        LiveCommand::CompleteDelegation { delegation_id, text }
            if delegation_id == "del-queued"
                && text.contains("already has queued user work")
    ));
}

#[test]
fn delegation_rolls_back_only_its_queue_row_when_dispatch_is_blocked() {
    let (mut app, id, mut cmd_rx) = app_with_active_live();
    let generation = app.live_runtime.state.generation().unwrap();

    // With an idle agent but a model switch in flight, generic prompt dispatch
    // enqueues the literal row and `maybe_drain_queue` returns no effect. The
    // Live wrapper must remove the exact newly minted row.
    app.agents
        .get_mut(&id)
        .unwrap()
        .session
        .model_switch_pending = true;
    let queue_id_before = app.agents[&id].session.next_queue_id;

    let effects = dispatch(
        Action::LiveDelegationSubmit {
            agent_id: id,
            text: "must not run later".into(),
            delegation_id: "del-blocked".into(),
            generation,
            draft: DraftSnapshot::default(),
        },
        &mut app,
    );

    assert!(effects.is_empty());
    assert!(
        app.agents[&id].session.pending_prompts.is_empty(),
        "the delegation's local row must be removed"
    );
    assert_eq!(
        app.agents[&id].session.next_queue_id,
        queue_id_before + 1,
        "the monotonic queue id may advance but must never be reused"
    );
    assert!(!app.live_runtime.has_delegation(generation, "del-blocked"));
    assert!(matches!(
        cmd_rx.try_recv().expect("blocked submit must complete"),
        LiveCommand::CompleteDelegation { delegation_id, .. }
            if delegation_id == "del-blocked"
    ));
}

#[test]
fn duplicate_delegation_created_id_produces_only_one_submit_action() {
    let (mut app, _id, _cmd_rx) = app_with_active_live();
    let event = LiveEvent::Delegation {
        id: "del-duplicate".into(),
        content: vec!["do this once".into()],
    };

    let (first_draw, first_actions) =
        crate::live::handle::handle_live_event(&mut app, event.clone());
    let (second_draw, second_actions) = crate::live::handle::handle_live_event(&mut app, event);

    assert!(first_draw);
    assert_eq!(first_actions.len(), 1);
    assert!(!second_draw);
    assert!(
        second_actions.is_empty(),
        "duplicate id must not submit twice"
    );
}

#[test]
fn assistant_final_flushes_merged_text_once_and_keeps_finalized_state() {
    use crate::scrollback::block::RenderBlock;

    let (mut app, id, _cmd_rx) = app_with_active_live();
    let before = app.agents[&id].scrollback.len();

    crate::live::handle::handle_live_event(
        &mut app,
        LiveEvent::Transcript {
            kind: TranscriptKind::Output,
            text: "Hello world, done.".into(),
        },
    );
    assert_eq!(
        app.agents[&id].scrollback.len(),
        before + 1,
        "first output delta creates one transient Live block"
    );
    let (_, streaming) = app.agents[&id]
        .scrollback
        .iter_entries()
        .last()
        .expect("Live streaming block");
    assert!(streaming.is_running);
    let RenderBlock::AgentMessage(streaming_message) = &streaming.block else {
        panic!("expected streaming Live assistant block");
    };
    assert!(streaming_message.text().contains("Hello world, done."));
    assert!(app.live_runtime.assistant_transcript_entry.is_some());

    crate::live::handle::handle_live_event(
        &mut app,
        LiveEvent::Turn {
            role: LiveRole::Assistant,
            // The streaming current is longer than this final snapshot and
            // must be the text persisted to scrollback.
            transcript: "Hello world".into(),
        },
    );

    assert_eq!(app.agents[&id].scrollback.len(), before + 1);
    let (_, last) = app.agents[&id]
        .scrollback
        .iter_entries()
        .last()
        .expect("Live final block");
    let RenderBlock::AgentMessage(message) = &last.block else {
        panic!("expected Live assistant agent-message block");
    };
    assert!(message.text().contains("Hello world, done."));
    assert!(!last.is_running, "turn.done finalizes the transient block");
    assert_eq!(
        app.live_runtime.visualizer.assistant_transcript,
        "Hello world, done."
    );
    assert!(app.live_runtime.visualizer.assistant_flushed);
    assert!(app.live_runtime.assistant_transcript_entry.is_none());

    // A repeated final frame from the same turn is ignored. The visualizer
    // retains finalized state instead of resetting it after the first flush.
    crate::live::handle::handle_live_event(
        &mut app,
        LiveEvent::Turn {
            role: LiveRole::Assistant,
            transcript: "Hello world".into(),
        },
    );
    assert_eq!(app.agents[&id].scrollback.len(), before + 1);
    assert!(app.live_runtime.visualizer.assistant_flushed);
}

#[test]
fn live_reset_finalizes_partial_assistant_transcript() {
    let (mut app, id, _cmd_rx) = app_with_active_live();
    crate::live::handle::handle_live_event(
        &mut app,
        LiveEvent::Transcript {
            kind: TranscriptKind::Output,
            text: "partial before disconnect".into(),
        },
    );
    let entry_id = app
        .live_runtime
        .assistant_transcript_entry
        .expect("streaming entry")
        .entry_id;
    assert!(
        app.agents[&id]
            .scrollback
            .get_by_id(entry_id)
            .unwrap()
            .is_running
    );

    app.live_reset();

    assert!(
        !app.agents[&id]
            .scrollback
            .get_by_id(entry_id)
            .unwrap()
            .is_running
    );
    assert!(app.live_runtime.assistant_transcript_entry.is_none());
    assert!(matches!(app.live_runtime.state, LiveState::Idle));
}

#[test]
fn delegation_not_accepted_leaves_no_queued_delegation() {
    // When the prompt is not accepted (no matching Effect::SendPrompt), the
    // delegation must be marked terminal, an explicit failure
    // CompleteDelegation enqueued, and NO delegation registered — so no
    // untracked work executes later. We force the not-accepted path by making
    // the active view NOT the bound agent (the delegation validates the active
    // view is the bound agent and fails fast).
    let (mut app, id, mut cmd_rx) = app_with_active_live();
    let generation = app.live_runtime.state.generation().unwrap();

    // Navigate away from the bound agent so the delegation is rejected.
    app.active_view = ActiveView::Welcome;

    let effects = dispatch(
        Action::LiveDelegationSubmit {
            agent_id: id,
            text: "orphan".into(),
            delegation_id: "del-orphan".into(),
            generation,
            draft: DraftSnapshot::default(),
        },
        &mut app,
    );

    // No SendPrompt effect (the delegation was not accepted).
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::SendPrompt { .. })),
        "rejected delegation must not emit SendPrompt"
    );

    // No delegation registered → no untracked work.
    assert!(
        !app.live_runtime.has_delegation(generation, "del-orphan"),
        "rejected delegation must not be registered"
    );

    // No untracked local queue entry.
    let leaked = app.agents[&id]
        .session
        .pending_prompts
        .iter()
        .any(|p| p.text == "orphan");
    assert!(
        !leaked,
        "rejected delegation must leave no local queue entry"
    );

    // No Live command was enqueued for the orphan (the early-reject path marks
    // terminal but does not send a CompleteDelegation because no delegation was
    // ever registered with the broker).
    assert!(
        cmd_rx.try_recv().is_err(),
        "no Live command must be sent for an early-rejected delegation"
    );
}

#[test]
fn live_stop_completes_each_active_delegation_before_shutdown() {
    let (mut app, id, mut cmd_rx) = app_with_active_live();
    let generation = app.live_runtime.state.generation().unwrap();
    let session_id = app.live_runtime.state.session_id().unwrap().to_string();
    assert!(
        app.live_runtime
            .broker
            .register_delegation("del-stop".into())
    );
    app.live_runtime.register_delegation(
        generation,
        "del-stop".into(),
        id,
        session_id,
        "pid-stop".into(),
    );

    let effects = dispatch(Action::LiveStop, &mut app);
    assert!(effects.is_empty());
    assert!(matches!(app.live_runtime.state, LiveState::Idle));

    let commands: Vec<_> = std::iter::from_fn(|| cmd_rx.try_recv().ok()).collect();
    assert!(commands.iter().any(|command| matches!(
        command,
        LiveCommand::CompleteDelegation { delegation_id, text }
            if delegation_id == "del-stop"
                && text.contains("cancelled because Live stopped")
    )));
    assert!(
        commands
            .iter()
            .any(|command| matches!(command, LiveCommand::Shutdown))
    );
}

// ── Centralized failure rail (on_prompt_failed) ─────────────────────────────

#[test]
fn on_prompt_failed_enqueues_one_complete_delegation_and_marks_terminal() {
    use crate::live::acp_bridge::on_prompt_failed;

    let (mut app, id, mut cmd_rx) = app_with_active_live();
    let generation = app.live_runtime.state.generation().unwrap();
    let session_id = app.live_runtime.state.session_id().unwrap().to_string();

    // Register a delegation the broker can correlate.
    app.live_runtime
        .broker
        .register_delegation("del-fail".into());
    app.live_runtime.register_delegation(
        generation,
        "del-fail".into(),
        id,
        session_id.clone(),
        "pid-fail".into(),
    );

    // The central failure rail: one call, one observe_failure, one
    // CompleteDelegation carrying the wrapped failure text.
    on_prompt_failed(&mut app, &session_id, "pid-fail", "Delegation failed: boom");

    // Exactly one CompleteDelegation was enqueued with the wrapped text.
    let mut seen_complete = 0;
    while let Ok(cmd) = cmd_rx.try_recv() {
        if let LiveCommand::CompleteDelegation {
            delegation_id,
            text,
        } = cmd
        {
            assert_eq!(delegation_id, "del-fail");
            assert!(
                text.starts_with("\"Agent Final Message\":"),
                "failure final must be wrapped, got: {text}"
            );
            assert!(
                text.contains("Delegation failed: boom"),
                "failure final must carry the message, got: {text}"
            );
            seen_complete += 1;
        }
    }
    assert_eq!(
        seen_complete, 1,
        "exactly one CompleteDelegation must be enqueued (not zero, not duplicated)"
    );

    // The registry delegation is marked terminal.
    assert!(
        app.live_runtime
            .is_delegation_terminal(generation, "del-fail"),
        "the central rail must mark the registry delegation terminal"
    );
}

#[test]
fn on_prompt_failed_duplicate_call_emits_no_second_complete() {
    use crate::live::acp_bridge::on_prompt_failed;

    let (mut app, id, mut cmd_rx) = app_with_active_live();
    let generation = app.live_runtime.state.generation().unwrap();
    let session_id = app.live_runtime.state.session_id().unwrap().to_string();

    app.live_runtime
        .broker
        .register_delegation("del-dup".into());
    app.live_runtime.register_delegation(
        generation,
        "del-dup".into(),
        id,
        session_id.clone(),
        "pid-dup".into(),
    );

    // First call enqueues the failure final.
    on_prompt_failed(&mut app, &session_id, "pid-dup", "Delegation cancelled");
    // Drain whatever was enqueued.
    while cmd_rx.try_recv().is_ok() {}

    // Duplicate call (e.g. the durable + legacy rails both firing) must NOT
    // enqueue a second CompleteDelegation — the broker is idempotent and the
    // registry is already terminal, so the rail's `decision.mark_terminal` is
    // empty.
    on_prompt_failed(&mut app, &session_id, "pid-dup", "Delegation cancelled");
    let mut second = 0;
    while let Ok(cmd) = cmd_rx.try_recv() {
        if matches!(cmd, LiveCommand::CompleteDelegation { .. }) {
            second += 1;
        }
    }
    assert_eq!(
        second, 0,
        "duplicate failure call must not emit a second CompleteDelegation"
    );
}

#[test]
fn on_prompt_failed_noop_when_live_inactive() {
    use crate::live::acp_bridge::on_prompt_failed;

    let (mut app, _id, mut cmd_rx) = app_with_active_live();
    // Drop Live back to Idle WITHOUT sending a Shutdown (teardown would
    // enqueue a Shutdown on the channel, which is legitimate and unrelated to
    // the rail). The rail must gate on `live_active()` and send nothing.
    app.live_runtime.state = LiveState::Idle;

    on_prompt_failed(&mut app, "any", "pid", "msg");
    assert!(
        cmd_rx.try_recv().is_err(),
        "on_prompt_failed must not send when Live is inactive"
    );
}
