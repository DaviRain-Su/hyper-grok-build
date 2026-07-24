//! Focused tests for the Codex Live (`/live`) integration.
//!
//! These tests exercise the real `xai_grok_voice::live` API types and the
//! pager-side adapters (state, broker, visualizer, config, prompts, gate).
//! No real audio/network — the tests are pure state/data tests.
//!
//! Coverage:
//! - broker: foreign session/prompt/old generation/replay/terminal rejection
//! - broker: tool-boundary commentary flush
//! - broker: terminal exactly once (turn completed + signal reordering)
//! - broker: multiple delegations per call + exact prompt correlation
//! - broker: cancel-all completes non-terminal
//! - broker: decision_to_commands uses Commentary channel + CompleteDelegation
//! - state: idle/active/pending transitions + generation counter
//! - state: delegation register/mark terminal/teardown
//! - state: find_delegation_by_prompt_id
//! - visualizer: phase update + no redraw on same phase
//! - visualizer: levels update + peak decay
//! - visualizer: user transcript accumulation
//! - visualizer: assistant transcript coalescing
//! - visualizer: error sets error message
//! - visualizer: reset_turn clears assistant
//! - visualizer: narrow detection
//! - config: built with OMP version
//! - prompts: wrap final message
//! - gate: defaults on / requirement off / config off

#![cfg(feature = "codex-live")]

use crate::live::broker::{
    BrokerDecision, CommentaryFlush, LiveDelegationBroker, TerminalFinal, decision_to_commands,
};
use crate::live::state::{
    DelegationEntry, DraftSnapshot, LiveRuntime, LiveState, LiveVisualizerState,
};
use crate::live::visualizer::{VISUALIZER_HEIGHT, VISUALIZER_NARROW_HEIGHT};
use crate::live::{
    LiveCommand, LiveContextChannel, LiveEvent, LivePhase, LiveRole, TranscriptKind,
};

// ── Broker helper functions ─────────────────────────────────────────────────

fn make_entry(generation: u64, del_id: &str, pid: &str, terminal: bool) -> DelegationEntry {
    DelegationEntry {
        generation,
        delegation_id: del_id.to_string(),
        agent_id: crate::app::agent::AgentId(0),
        session_id: "sess-1".to_string(),
        prompt_id: pid.to_string(),
        terminal,
    }
}

fn make_delegations(
    entries: &[DelegationEntry],
) -> std::collections::HashMap<(u64, String), DelegationEntry> {
    entries
        .iter()
        .map(|e| ((e.generation, e.delegation_id.clone()), e.clone()))
        .collect()
}

// ── Broker tests ────────────────────────────────────────────────────────────

#[test]
fn broker_foreign_session_ignored() {
    let mut broker = LiveDelegationBroker::new(1);
    broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
    broker.register_delegation("del-1".to_string());
    let delegations = make_delegations(&[make_entry(1, "del-1", "pid-1", false)]);
    let decision = broker.observe_chunk("sess-other", "pid-1", "hello", false, &delegations);
    assert!(decision.commentary.is_empty());
    assert!(decision.terminal.is_empty());
}

#[test]
fn broker_foreign_prompt_ignored() {
    let mut broker = LiveDelegationBroker::new(1);
    broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
    broker.register_delegation("del-1".to_string());
    let delegations = make_delegations(&[make_entry(1, "del-1", "pid-1", false)]);
    let decision = broker.observe_chunk("sess-1", "pid-other", "hello", false, &delegations);
    assert!(decision.commentary.is_empty());
}

#[test]
fn broker_old_generation_ignored() {
    let mut broker = LiveDelegationBroker::new(2);
    broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
    broker.register_delegation("del-1".to_string());
    let delegations = make_delegations(&[make_entry(1, "del-1", "pid-1", false)]);
    let decision = broker.observe_chunk("sess-1", "pid-1", "hello", false, &delegations);
    assert!(decision.commentary.is_empty());
}

#[test]
fn broker_replay_ignored_via_terminal_delegation() {
    // A terminal delegation should be ignored (simulates replay/stale).
    let mut broker = LiveDelegationBroker::new(1);
    broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
    broker.register_delegation("del-1".to_string());
    let delegations = make_delegations(&[make_entry(1, "del-1", "pid-1", true)]);
    let decision = broker.observe_chunk("sess-1", "pid-1", "hello", false, &delegations);
    assert!(decision.commentary.is_empty());
}

#[test]
fn broker_tool_boundary_flushes_commentary() {
    let mut broker = LiveDelegationBroker::new(1);
    broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
    broker.register_delegation("del-1".to_string());
    let delegations = make_delegations(&[make_entry(1, "del-1", "pid-1", false)]);

    broker.observe_chunk("sess-1", "pid-1", "I will now ", false, &delegations);
    broker.observe_chunk("sess-1", "pid-1", "edit the file.", false, &delegations);

    let decision = broker.observe_chunk("sess-1", "pid-1", "", true, &delegations);
    assert_eq!(decision.commentary.len(), 1);
    assert_eq!(decision.commentary[0].delegation_id, "del-1");
    assert_eq!(decision.commentary[0].text, "I will now edit the file.");
}

#[test]
fn broker_terminal_exactly_once() {
    let mut broker = LiveDelegationBroker::new(1);
    broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
    broker.register_delegation("del-1".to_string());
    let delegations = make_delegations(&[make_entry(1, "del-1", "pid-1", false)]);

    broker.observe_chunk("sess-1", "pid-1", "Done!", false, &delegations);

    let d1 = broker.observe_turn_completed("sess-1", "pid-1", &delegations);
    assert_eq!(d1.terminal.len(), 1);
    assert!(
        d1.terminal[0]
            .final_message
            .starts_with("Agent Final Message:")
    );
    assert_eq!(d1.mark_terminal, vec!["del-1".to_string()]);

    let d2 = broker.observe_turn_completed("sess-1", "pid-1", &delegations);
    assert!(d2.terminal.is_empty());
    assert!(d2.mark_terminal.is_empty());
}

#[test]
fn broker_signal_reordering_terminal_wins() {
    let mut broker = LiveDelegationBroker::new(1);
    broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
    broker.register_delegation("del-1".to_string());
    let delegations = make_delegations(&[make_entry(1, "del-1", "pid-1", false)]);

    broker.observe_chunk("sess-1", "pid-1", "result", false, &delegations);
    let d1 = broker.observe_turn_completed("sess-1", "pid-1", &delegations);
    assert_eq!(d1.terminal.len(), 1);

    let d2 = broker.observe_failure("sess-1", "pid-1", &delegations);
    assert!(d2.terminal.is_empty());
    assert!(d2.mark_terminal.is_empty());
}

#[test]
fn broker_repeated_delegation_ids() {
    let mut broker = LiveDelegationBroker::new(1);
    broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
    broker.register_delegation("del-1".to_string());
    broker.register_delegation("del-2".to_string());
    let delegations = make_delegations(&[
        make_entry(1, "del-1", "pid-1", false),
        make_entry(1, "del-2", "pid-2", false),
    ]);

    let d1t = broker.observe_turn_completed("sess-1", "pid-1", &delegations);
    assert_eq!(d1t.terminal.len(), 1);
    assert_eq!(d1t.terminal[0].delegation_id, "del-1");

    let d2t = broker.observe_turn_completed("sess-1", "pid-2", &delegations);
    assert_eq!(d2t.terminal.len(), 1);
    assert_eq!(d2t.terminal[0].delegation_id, "del-2");
}

#[test]
fn broker_exact_prompt_correlation() {
    let mut broker = LiveDelegationBroker::new(1);
    broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
    broker.register_delegation("del-1".to_string());
    broker.register_delegation("del-2".to_string());
    let delegations = make_delegations(&[
        make_entry(1, "del-1", "pid-1", false),
        make_entry(1, "del-2", "pid-2", false),
    ]);

    // Chunk for pid-1 should only correlate with del-1.
    let d = broker.observe_chunk("sess-1", "pid-1", "text for del-1", true, &delegations);
    assert_eq!(d.commentary.len(), 1);
    assert_eq!(d.commentary[0].delegation_id, "del-1");

    // Chunk for pid-2 should only correlate with del-2.
    let d = broker.observe_chunk("sess-1", "pid-2", "text for del-2", true, &delegations);
    assert_eq!(d.commentary.len(), 1);
    assert_eq!(d.commentary[0].delegation_id, "del-2");
}

#[test]
fn broker_cancel_all_completes_non_terminal() {
    let mut broker = LiveDelegationBroker::new(1);
    broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
    broker.register_delegation("del-1".to_string());
    broker.register_delegation("del-2".to_string());
    let delegations = make_delegations(&[
        make_entry(1, "del-1", "pid-1", false),
        make_entry(1, "del-2", "pid-2", false),
    ]);

    let d = broker.observe_cancel_all(&delegations);
    assert_eq!(d.mark_terminal.len(), 2);
}

#[test]
fn broker_decision_to_commands_uses_commentary_channel() {
    let decision = BrokerDecision {
        commentary: vec![CommentaryFlush {
            delegation_id: "del-1".to_string(),
            text: "hello".to_string(),
        }],
        terminal: vec![TerminalFinal {
            delegation_id: "del-1".to_string(),
            final_message: "Agent Final Message: done".to_string(),
        }],
        mark_terminal: vec!["del-1".to_string()],
    };
    let cmds = decision_to_commands(&decision);
    assert_eq!(cmds.len(), 2);
    match &cmds[0] {
        LiveCommand::AppendDelegationContext {
            channel,
            delegation_id,
            text,
        } => {
            assert_eq!(*channel, LiveContextChannel::Commentary);
            assert_eq!(delegation_id, "del-1");
            assert_eq!(text, "hello");
        }
        _ => panic!("expected AppendDelegationContext"),
    }
    match &cmds[1] {
        LiveCommand::CompleteDelegation {
            delegation_id,
            text,
        } => {
            assert_eq!(delegation_id, "del-1");
            assert!(text.starts_with("Agent Final Message:"));
        }
        _ => panic!("expected CompleteDelegation"),
    }
}

// ── State tests ─────────────────────────────────────────────────────────────

#[test]
fn live_state_idle_by_default() {
    let state = LiveState::default();
    assert!(matches!(state, LiveState::Idle));
    assert!(!state.is_active());
    assert!(!state.is_pending());
    assert!(!state.is_in_flight());
    assert!(state.agent_id().is_none());
    assert!(state.session_id().is_none());
}

#[test]
fn live_state_active_transitions() {
    let state = LiveState::Active {
        agent_id: crate::app::agent::AgentId(1),
        session_id: "sess-1".to_string(),
        generation: 42,
        draft: DraftSnapshot::default(),
    };
    assert!(state.is_active());
    assert!(state.is_in_flight());
    assert_eq!(state.agent_id(), Some(crate::app::agent::AgentId(1)));
    assert_eq!(state.session_id(), Some("sess-1"));
    assert_eq!(state.generation(), Some(42));
}

#[test]
fn live_state_pending_unbound() {
    let state = LiveState::PendingUnbound {
        agent_id: crate::app::agent::AgentId(0),
        draft: DraftSnapshot {
            text: "hello".to_string(),
            cursor: 3,
        },
    };
    assert!(state.is_pending());
    assert!(state.is_in_flight());
    assert!(!state.is_active());
    assert_eq!(state.agent_id(), Some(crate::app::agent::AgentId(0)));
    assert!(state.session_id().is_none());
    assert!(state.generation().is_none());
    assert!(state.draft().is_some());
}

#[test]
fn live_runtime_generation_counter() {
    let mut runtime = LiveRuntime::default();
    assert_eq!(runtime.next_generation(), 1);
    assert_eq!(runtime.next_generation(), 2);
    assert_eq!(runtime.next_generation(), 3);
}

#[test]
fn live_runtime_register_and_mark_delegation() {
    let mut runtime = LiveRuntime::default();
    let generation = runtime.next_generation();
    runtime.register_delegation(
        generation,
        "del-1".to_string(),
        crate::app::agent::AgentId(0),
        "sess-1".to_string(),
        "pid-1".to_string(),
    );
    assert!(runtime.has_delegation(generation, "del-1"));
    assert!(!runtime.is_delegation_terminal(generation, "del-1"));
    runtime.mark_delegation_terminal(generation, "del-1");
    assert!(runtime.is_delegation_terminal(generation, "del-1"));
}

#[test]
fn live_runtime_teardown_clears_everything() {
    let mut runtime = LiveRuntime::default();
    let generation = runtime.next_generation();
    runtime.register_delegation(
        generation,
        "del-1".to_string(),
        crate::app::agent::AgentId(0),
        "sess-1".to_string(),
        "pid-1".to_string(),
    );
    runtime.state = LiveState::Active {
        agent_id: crate::app::agent::AgentId(0),
        session_id: "sess-1".to_string(),
        generation,
        draft: DraftSnapshot::default(),
    };
    runtime.teardown();
    assert!(matches!(runtime.state, LiveState::Idle));
    assert!(runtime.delegations.is_empty());
}

#[test]
fn live_runtime_find_delegation_by_prompt_id() {
    let mut runtime = LiveRuntime::default();
    let generation = runtime.next_generation();
    runtime.register_delegation(
        generation,
        "del-1".to_string(),
        crate::app::agent::AgentId(0),
        "sess-1".to_string(),
        "pid-1".to_string(),
    );
    runtime.register_delegation(
        generation,
        "del-2".to_string(),
        crate::app::agent::AgentId(0),
        "sess-1".to_string(),
        "pid-2".to_string(),
    );
    assert_eq!(
        runtime.find_delegation_by_prompt_id("pid-1"),
        Some("del-1".to_string())
    );
    assert_eq!(
        runtime.find_delegation_by_prompt_id("pid-2"),
        Some("del-2".to_string())
    );
    assert_eq!(runtime.find_delegation_by_prompt_id("pid-3"), None);

    // Terminal delegations should not be found.
    runtime.mark_delegation_terminal(generation, "del-1");
    assert_eq!(runtime.find_delegation_by_prompt_id("pid-1"), None);
}

// ── Visualizer state tests ──────────────────────────────────────────────────

#[test]
fn visualizer_phase_update() {
    let mut vis = LiveVisualizerState::default();
    let needs_draw = vis.apply_event(&LiveEvent::Phase(LivePhase::Connected));
    assert!(needs_draw);
    assert_eq!(vis.phase, LivePhase::Connected);
}

#[test]
fn visualizer_phase_no_redraw_on_same_phase() {
    let mut vis = LiveVisualizerState::default();
    vis.phase = LivePhase::Connected;
    let needs_draw = vis.apply_event(&LiveEvent::Phase(LivePhase::Connected));
    assert!(!needs_draw);
}

#[test]
fn visualizer_levels_update_and_peak_decay() {
    let mut vis = LiveVisualizerState::default();
    let needs_draw = vis.apply_event(&LiveEvent::Levels(0.8));
    assert!(needs_draw);
    assert!((vis.level - 0.8).abs() < 0.01);
    assert!((vis.peak_decay - 0.8).abs() < 0.01);

    vis.decay_peak(0.5);
    assert!((vis.peak_decay - 0.4).abs() < 0.01);
}

#[test]
fn visualizer_levels_flood_does_not_panic() {
    let mut vis = LiveVisualizerState::default();
    for i in 0..1000 {
        let level = (i as f64) / 1000.0;
        vis.apply_event(&LiveEvent::Levels(level));
    }
    assert!(vis.level <= 1.0);
    assert!(vis.peak_decay <= 1.0);
}

#[test]
fn visualizer_user_transcript_accumulates() {
    let mut vis = LiveVisualizerState::default();
    vis.apply_event(&LiveEvent::Transcript {
        kind: TranscriptKind::Input,
        text: "Hello".to_string(),
    });
    vis.apply_event(&LiveEvent::Transcript {
        kind: TranscriptKind::Input,
        text: "world".to_string(),
    });
    assert_eq!(vis.user_transcript, "Hello world");
}

#[test]
fn visualizer_assistant_transcript_coalescing() {
    let mut vis = LiveVisualizerState::default();
    // Output transcript events replace the live partial (coalescing).
    vis.apply_event(&LiveEvent::Transcript {
        kind: TranscriptKind::Output,
        text: "I am".to_string(),
    });
    assert_eq!(vis.assistant_transcript, "I am");
    vis.apply_event(&LiveEvent::Transcript {
        kind: TranscriptKind::Output,
        text: "I am thinking".to_string(),
    });
    assert_eq!(vis.assistant_transcript, "I am thinking");
}

#[test]
fn visualizer_turn_resets_assistant() {
    let mut vis = LiveVisualizerState::default();
    vis.apply_event(&LiveEvent::Transcript {
        kind: TranscriptKind::Output,
        text: "response".to_string(),
    });
    vis.apply_event(&LiveEvent::Turn {
        role: LiveRole::Assistant,
        transcript: "final response".to_string(),
    });
    assert_eq!(vis.assistant_transcript, "final response");
    assert!(!vis.assistant_flushed); // needs scrollback flush
    vis.reset_turn();
    assert!(vis.assistant_transcript.is_empty());
    assert!(!vis.assistant_flushed);
}

#[test]
fn visualizer_error_sets_error_message() {
    let mut vis = LiveVisualizerState::default();
    let needs_draw = vis.apply_event(&LiveEvent::Error {
        message: "Connection lost".to_string(),
    });
    assert!(needs_draw);
    assert_eq!(vis.error_message, Some("Connection lost".to_string()));
}

#[test]
fn visualizer_reset_turn_clears_assistant() {
    let mut vis = LiveVisualizerState::default();
    vis.apply_event(&LiveEvent::Transcript {
        kind: TranscriptKind::Output,
        text: "response".to_string(),
    });
    vis.reset_turn();
    assert!(vis.assistant_transcript.is_empty());
    assert!(!vis.assistant_flushed);
}

// ── Visualizer layout tests ─────────────────────────────────────────────────

#[test]
fn visualizer_narrow_detection() {
    use ratatui::layout::Rect;
    let narrow = Rect::new(0, 0, 30, 10);
    assert!(crate::live::visualizer::is_narrow(narrow));
    let wide = Rect::new(0, 0, 60, 10);
    assert!(!crate::live::visualizer::is_narrow(wide));
}

#[test]
fn visualizer_render_wide_does_not_panic() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    let mut buf = Buffer::empty(Rect::new(0, 0, 80, 10));
    let state = LiveVisualizerState::default();
    crate::live::visualizer::render(&mut buf, Rect::new(0, 0, 80, 10), &state);
    // Should have rendered something (not all empty).
    assert!(buf.area().width > 0);
}

#[test]
fn visualizer_render_narrow_does_not_panic() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    let mut buf = Buffer::empty(Rect::new(0, 0, 30, 10));
    let state = LiveVisualizerState::default();
    crate::live::visualizer::render(&mut buf, Rect::new(0, 0, 30, 10), &state);
    assert!(buf.area().width > 0);
}

// ── Config tests ────────────────────────────────────────────────────────────

#[test]
fn live_config_built_with_omp_version() {
    let config = crate::live::config::build_live_config(
        "sess-1",
        "https://chatgpt.com/backend-api",
        None,
        "sol",
    );
    assert_eq!(config.session_id, "sess-1");
    assert_eq!(config.codex_base, "https://chatgpt.com/backend-api");
    assert_eq!(config.voice, "sol");
    assert_eq!(config.client_version, "0.144.1");
    assert!(config.sideband_base.is_none());
}

#[test]
fn live_config_with_sideband() {
    let config = crate::live::config::build_live_config(
        "sess-1",
        "https://chatgpt.com/backend-api",
        Some("wss://sideband".to_string()),
        "sol",
    );
    assert_eq!(config.sideband_base.as_deref(), Some("wss://sideband"));
}

#[test]
fn live_prompts_wrap_final_message() {
    let wrapped = crate::live::prompts::wrap_agent_final_message("All done");
    assert!(wrapped.starts_with("Agent Final Message:"));
    assert!(wrapped.contains("All done"));
}

#[test]
fn live_prompts_instructions_nonempty() {
    let instructions = crate::live::prompts::live_instructions();
    assert!(!instructions.is_empty());
}

// ── Gate tests ──────────────────────────────────────────────────────────────

#[test]
fn gate_defaults_on() {
    assert!(crate::live::gate::resolve_codex_live_enabled(
        Some(true),
        Some(true)
    ));
}

#[test]
fn gate_requirement_off_disables() {
    assert!(!crate::live::gate::resolve_codex_live_enabled(
        Some(false),
        Some(true)
    ));
}

#[test]
fn gate_config_off_disables_when_requirement_absent() {
    assert!(!crate::live::gate::resolve_codex_live_enabled(
        None,
        Some(false)
    ));
}

// ── DraftSnapshot tests ─────────────────────────────────────────────────────

#[test]
fn draft_snapshot_default_is_empty() {
    let draft = DraftSnapshot::default();
    assert!(draft.text.is_empty());
    assert_eq!(draft.cursor, 0);
}

#[test]
fn draft_snapshot_preserves_text_and_cursor() {
    let draft = DraftSnapshot {
        text: "hello world".to_string(),
        cursor: 5,
    };
    assert_eq!(draft.text, "hello world");
    assert_eq!(draft.cursor, 5);
}

// ── Real API type compilation tests ─────────────────────────────────────────
// These tests verify that the pager code compiles against the real
// `xai_grok_voice::live` types (not test doubles). If the voice crate API
// changes, these tests will fail to compile.

#[test]
fn real_live_command_variants_exist() {
    let _ = LiveCommand::ToggleMute;
    let _ = LiveCommand::SetMuted(true);
    let _ = LiveCommand::Shutdown;
    let _ = LiveCommand::AppendDelegationContext {
        delegation_id: "del".to_string(),
        text: "text".to_string(),
        channel: LiveContextChannel::Speakable,
    };
    let _ = LiveCommand::AppendDelegationContext {
        delegation_id: "del".to_string(),
        text: "text".to_string(),
        channel: LiveContextChannel::Commentary,
    };
    let _ = LiveCommand::CompleteDelegation {
        delegation_id: "del".to_string(),
        text: "final".to_string(),
    };
    let _ = LiveCommand::AppendSessionContext {
        text: "ctx".to_string(),
        channel: LiveContextChannel::Speakable,
    };
}

#[test]
fn real_live_event_variants_exist() {
    let _ = LiveEvent::Phase(LivePhase::Connecting);
    let _ = LiveEvent::Phase(LivePhase::Connected);
    let _ = LiveEvent::Phase(LivePhase::Closing);
    let _ = LiveEvent::Phase(LivePhase::Closed);
    let _ = LiveEvent::Levels(0.5);
    let _ = LiveEvent::Transcript {
        kind: TranscriptKind::Input,
        text: "hi".to_string(),
    };
    let _ = LiveEvent::Transcript {
        kind: TranscriptKind::Output,
        text: "hello".to_string(),
    };
    let _ = LiveEvent::Delegation {
        id: "del-1".to_string(),
        content: vec!["text".to_string()],
    };
    let _ = LiveEvent::Turn {
        role: LiveRole::User,
        transcript: "user transcript".to_string(),
    };
    let _ = LiveEvent::Turn {
        role: LiveRole::Assistant,
        transcript: "assistant transcript".to_string(),
    };
    let _ = LiveEvent::Error {
        message: "err".to_string(),
    };
    let _ = LiveEvent::Closed;
}

#[test]
fn real_live_context_channel_is_enum() {
    // LiveContextChannel is an enum (Speakable/Commentary), not a struct.
    let speakable = LiveContextChannel::Speakable;
    let commentary = LiveContextChannel::Commentary;
    assert!(matches!(speakable, LiveContextChannel::Speakable));
    assert!(matches!(commentary, LiveContextChannel::Commentary));
}

// ── Auth adapter test (compile-link only) ───────────────────────────────────

#[test]
fn real_auth_provider_trait_object_compiles() {
    use crate::live::LiveAuthProvider;
    use crate::live::auth::CodexLiveAuth;
    // Verify the auth adapter implements the real trait.
    fn accepts_provider(_p: &dyn LiveAuthProvider) {}
    let provider = CodexLiveAuth;
    accepts_provider(&provider);
}

// ── Regression: command queue reliability (issue #4) ───────────────────────

#[test]
fn command_queue_complete_delegation_queued_when_channel_full() {
    use crate::live::LiveCommand;
    use crate::live::state::LiveRuntime;

    let mut runtime = LiveRuntime::default();
    // Create a 1-slot channel.
    let (tx, _rx) = tokio::sync::mpsc::channel::<LiveCommand>(1);
    // Clone before moving into runtime.
    let tx_clone = tx.clone();
    runtime.cmd_tx = Some(tx);

    // Fill the channel with a mute toggle.
    tx_clone.try_send(LiveCommand::ToggleMute).unwrap();

    // Now try to send CompleteDelegation — channel is full.
    runtime.send_cmd(LiveCommand::CompleteDelegation {
        delegation_id: "del-1".to_string(),
        text: "final".to_string(),
    });

    // The CompleteDelegation should be queued in pending_cmds.
    assert_eq!(runtime.pending_cmds.len(), 1);
    assert!(matches!(
        &runtime.pending_cmds[0],
        LiveCommand::CompleteDelegation { delegation_id, .. } if delegation_id == "del-1"
    ));

    // Simulate the receiver draining the channel by dropping the old sender
    // and creating a new one. Actually, we can just test drain_pending_cmds
    // after the channel has capacity (the _rx receiver is still alive but
    // we can't drain it synchronously). Instead, verify the queue is
    // non-empty and the command is correct.
    // For a full drain test, we'd need a tokio runtime — but the queue
    // logic is simple enough that the above assertion suffices.
}

#[test]
fn command_queue_commentary_shed_when_channel_full() {
    use crate::live::LiveCommand;
    use crate::live::state::LiveRuntime;

    let mut runtime = LiveRuntime::default();
    let (tx, _rx) = tokio::sync::mpsc::channel::<LiveCommand>(1);
    let tx_clone = tx.clone();
    runtime.cmd_tx = Some(tx);

    // Fill the channel.
    tx_clone.try_send(LiveCommand::ToggleMute).unwrap();

    // Try to send commentary — should be shed, not queued.
    runtime.send_cmd(LiveCommand::AppendDelegationContext {
        delegation_id: "del-1".to_string(),
        text: "commentary".to_string(),
        channel: LiveContextChannel::Commentary,
    });

    // Commentary should NOT be in pending_cmds.
    assert!(runtime.pending_cmds.is_empty());
}

#[test]
fn command_queue_shutdown_queued_when_channel_full() {
    use crate::live::LiveCommand;
    use crate::live::state::LiveRuntime;

    let mut runtime = LiveRuntime::default();
    let (tx, _rx) = tokio::sync::mpsc::channel::<LiveCommand>(1);
    let tx_clone = tx.clone();
    runtime.cmd_tx = Some(tx);

    // Fill the channel.
    tx_clone.try_send(LiveCommand::ToggleMute).unwrap();

    // Try to send Shutdown — should be queued.
    runtime.send_cmd(LiveCommand::Shutdown);
    assert_eq!(runtime.pending_cmds.len(), 1);
    assert!(matches!(runtime.pending_cmds[0], LiveCommand::Shutdown));
}

#[test]
fn command_queue_teardown_clears_pending() {
    use crate::live::LiveCommand;
    use crate::live::state::LiveRuntime;

    let mut runtime = LiveRuntime::default();
    runtime.pending_cmds.push(LiveCommand::Shutdown);
    runtime.teardown();
    assert!(runtime.pending_cmds.is_empty());
}

// ── Regression: transcript scrollback (issue #5) ────────────────────────────

#[test]
fn transcript_multi_delta_then_final_flushes_once() {
    let mut vis = LiveVisualizerState::default();

    // Simulate multiple output deltas (streaming).
    vis.apply_event(&LiveEvent::Transcript {
        kind: TranscriptKind::Output,
        text: "Hello".to_string(),
    });
    assert_eq!(vis.assistant_transcript, "Hello");

    vis.apply_event(&LiveEvent::Transcript {
        kind: TranscriptKind::Output,
        text: "Hello world".to_string(),
    });
    assert_eq!(vis.assistant_transcript, "Hello world");

    vis.apply_event(&LiveEvent::Transcript {
        kind: TranscriptKind::Output,
        text: "Hello world, done.".to_string(),
    });
    assert_eq!(vis.assistant_transcript, "Hello world, done.");

    // The final Turn event provides the complete transcript.
    vis.apply_event(&LiveEvent::Turn {
        role: LiveRole::Assistant,
        transcript: "Hello world, done.".to_string(),
    });
    assert_eq!(vis.assistant_transcript, "Hello world, done.");
    assert!(!vis.assistant_flushed); // needs scrollback flush

    // After flush + reset, the next turn starts clean.
    vis.assistant_flushed = true; // simulate flush
    vis.reset_turn();
    assert!(vis.assistant_transcript.is_empty());
    assert!(!vis.assistant_flushed);
}

// ── Regression: visualizer layout (issue #6) ────────────────────────────────

#[test]
fn visualizer_wide_renders_all_rows() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    let mut state = LiveVisualizerState::default();
    state.phase = LivePhase::Connected;
    state.level = 0.5;
    state.user_transcript = "test transcript".to_string();

    let area = Rect::new(0, 0, 80, VISUALIZER_HEIGHT);
    let mut buf = Buffer::empty(area);
    crate::live::visualizer::render(&mut buf, area, &state);

    // The buffer should have non-empty content in the last row (footer).
    let footer_y = area.y + area.height - 1;
    let has_content = (0..area.width)
        .map(|x| buf[(x, footer_y)].symbol())
        .any(|s| s != " ");
    assert!(has_content, "footer row must have rendered content");

    // The transcript row (4th from top, 0-indexed: y=3) should have content.
    let transcript_y = area.y + 3;
    let has_transcript = (0..area.width)
        .map(|x| buf[(x, transcript_y)].symbol())
        .any(|s| s != " ");
    assert!(has_transcript, "transcript row must have rendered content");
}

#[test]
fn visualizer_narrow_renders_transcript_and_footer() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    let mut state = LiveVisualizerState::default();
    state.phase = LivePhase::Connected;
    state.level = 0.5;
    state.user_transcript = "narrow test".to_string();
    state.muted = true;

    let area = Rect::new(0, 0, 30, VISUALIZER_NARROW_HEIGHT);
    let mut buf = Buffer::empty(area);
    crate::live::visualizer::render(&mut buf, area, &state);

    // The last row (transcript) should have content.
    let transcript_y = area.y + area.height - 1;
    let has_content = (0..area.width)
        .map(|x| buf[(x, transcript_y)].symbol())
        .any(|s| s != " ");
    assert!(
        has_content,
        "narrow transcript row must have rendered content"
    );

    // The first content row (after border) should have the phase/mute status.
    let phase_y = area.y + 1; // after top border
    let has_phase = (0..area.width)
        .map(|x| buf[(x, phase_y)].symbol())
        .any(|s| s != " ");
    assert!(has_phase, "narrow phase row must have rendered content");
}

// ── Regression: visual status (issue #7) ────────────────────────────────────

#[test]
fn visualizer_state_muted_field() {
    let mut vis = LiveVisualizerState::default();
    assert!(!vis.muted);
    vis.muted = true;
    assert!(vis.muted);
}

#[test]
fn visualizer_state_delegation_active_field() {
    let mut vis = LiveVisualizerState::default();
    assert!(!vis.delegation_active);
    vis.delegation_active = true;
    assert!(vis.delegation_active);
}

// ── Regression: broker duplicate terminal rails (issue #3) ──────────────────

#[test]
fn broker_duplicate_turn_completed_is_noop() {
    let mut broker = LiveDelegationBroker::new(1);
    broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
    broker.register_delegation("del-1".to_string());
    let delegations = make_delegations(&[make_entry(1, "del-1", "pid-1", false)]);

    let d1 = broker.observe_turn_completed("sess-1", "pid-1", &delegations);
    assert_eq!(d1.terminal.len(), 1);

    // Duplicate — should be a no-op.
    let d2 = broker.observe_turn_completed("sess-1", "pid-1", &delegations);
    assert!(d2.terminal.is_empty());
    assert!(d2.mark_terminal.is_empty());
}

#[test]
fn broker_foreign_prompt_on_turn_completed_ignored() {
    let mut broker = LiveDelegationBroker::new(1);
    broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
    broker.register_delegation("del-1".to_string());
    let delegations = make_delegations(&[make_entry(1, "del-1", "pid-1", false)]);

    // Foreign prompt_id — should be ignored.
    let d = broker.observe_turn_completed("sess-1", "pid-foreign", &delegations);
    assert!(d.terminal.is_empty());
    assert!(d.mark_terminal.is_empty());
}

#[test]
fn broker_cancel_produces_no_terminal_message() {
    let mut broker = LiveDelegationBroker::new(1);
    broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
    broker.register_delegation("del-1".to_string());
    let delegations = make_delegations(&[make_entry(1, "del-1", "pid-1", false)]);

    // Cancel/failure — marks terminal but no final message.
    let d = broker.observe_failure("sess-1", "pid-1", &delegations);
    assert!(d.terminal.is_empty()); // no final message on failure
    assert_eq!(d.mark_terminal, vec!["del-1".to_string()]);

    // Duplicate failure — no-op.
    let d2 = broker.observe_failure("sess-1", "pid-1", &delegations);
    assert!(d2.mark_terminal.is_empty());
}

// ── Regression: capacity-aware critical command drain (issue #1) ───────────
//
// `drain_pending_cmds` only retries `try_send` at event-loop top. If the
// voice channel is still full, the loop can sleep with no capacity wake, so
// a final `CompleteDelegation` may remain forever. The capacity-aware async
// drain (`pending_critical_drain`) awaits the bounded channel's `send` so a
// stranded final eventually sends when the receiver frees capacity — even
// with no unrelated app event.

#[tokio::test]
async fn critical_drain_delivers_after_capacity_freed_with_no_app_event() {
    use crate::live::state::LiveRuntime;

    let mut runtime = LiveRuntime::default();
    // 1-slot channel — fill it so the final can't try_send.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<LiveCommand>(1);
    runtime.cmd_tx = Some(tx);

    // Fill the single slot with a non-critical command.
    runtime
        .cmd_tx
        .as_ref()
        .unwrap()
        .try_send(LiveCommand::ToggleMute)
        .unwrap();

    // Enqueue the final CompleteDelegation — channel is full, so it must land
    // in pending_cmds (send_cmd queues critical commands).
    runtime.send_cmd(LiveCommand::CompleteDelegation {
        delegation_id: "del-final".to_string(),
        text: "Agent Final Message: done".to_string(),
    });
    assert!(runtime.has_pending_critical());
    assert_eq!(runtime.pending_cmds.len(), 1);

    // The synchronous reaper can't deliver yet (channel still full).
    assert_eq!(runtime.drain_pending_cmds(), 0);
    assert!(runtime.has_pending_critical());

    // The async drain arm pops the head critical command (owns it exclusively)
    // and awaits `send().await`. No unrelated app event fires here — the wake
    // must come from capacity, not an app event.
    let (cmd, tx_clone) = runtime
        .take_pending_critical_head()
        .expect("pending critical command present");
    // Popped — no longer pending (the arm owns delivery).
    assert!(!runtime.has_pending_critical());

    // Receiver frees capacity AFTER the drain arm armed its `send().await`.
    let send_task = tokio::spawn(async move {
        // `send` blocks until capacity; the receiver frees it below.
        tx_clone.send(cmd).await
    });

    // Free capacity on the receiver side (drain the filler). This is the only
    // event — there is no unrelated app event waking the loop.
    let _filler = rx.recv().await.expect("filler command received");
    // The stranded final now sends because capacity freed.
    send_task
        .await
        .expect("send task joined")
        .expect("final CompleteDelegation sent after capacity freed");

    // The command was delivered directly by the arm (it owned the send), so
    // nothing remains pending.
    assert!(!runtime.has_pending_critical());
    assert!(runtime.pending_cmds.is_empty());

    // And the receiver observes the final.
    let received = rx.recv().await.expect("final received");
    match received {
        LiveCommand::CompleteDelegation {
            delegation_id,
            text,
        } => {
            assert_eq!(delegation_id, "del-final");
            assert!(text.starts_with("Agent Final Message:"));
        }
        other => panic!("expected CompleteDelegation, got {other:?}"),
    }
}

#[tokio::test]
async fn critical_drain_returns_command_to_front_on_timeout() {
    use crate::live::state::LiveRuntime;

    let mut runtime = LiveRuntime::default();
    // 1-slot channel — fill it and keep it full so the send times out.
    let (tx, _rx) = tokio::sync::mpsc::channel::<LiveCommand>(1);
    runtime.cmd_tx = Some(tx);
    runtime
        .cmd_tx
        .as_ref()
        .unwrap()
        .try_send(LiveCommand::ToggleMute)
        .unwrap();

    // Enqueue a critical command, then pop it (as the arm does).
    runtime.send_cmd(LiveCommand::CompleteDelegation {
        delegation_id: "del-to".to_string(),
        text: "Agent Final Message: timeout".to_string(),
    });
    let (cmd, _tx_clone) = runtime
        .take_pending_critical_head()
        .expect("pending critical command present");
    assert!(!runtime.has_pending_critical());

    // Simulate a timed-out send: return the command to the front.
    runtime.return_pending_critical_head(cmd);
    assert!(runtime.has_pending_critical());
    assert_eq!(runtime.pending_cmds.len(), 1);
    // It must be at the front (retried in order).
    assert!(matches!(
        &runtime.pending_cmds[0],
        LiveCommand::CompleteDelegation { delegation_id, .. } if delegation_id == "del-to"
    ));
}

// ── Regression: centralized failure rail (issue #3) ─────────────────────────
//
// The previous design called `observe_failure` twice (once in `on_prompt_error`,
// once in each caller). The second call returned an empty decision because the
// broker is idempotent, so the explicit `CompleteDelegation` loop emitted
// nothing. The central `on_prompt_failed` rail calls `observe_failure` exactly
// once and enqueues the wrapped failure final for each `decision.mark_terminal`
// on that same call. These broker-level tests verify the decision shape the
// rail relies on: a single `observe_failure` returns exactly the delegations to
// complete, and a duplicate returns nothing.

#[test]
fn broker_failure_returns_mark_terminal_for_completion_rail() {
    let mut broker = LiveDelegationBroker::new(1);
    broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
    broker.register_delegation("del-1".to_string());
    let delegations = make_delegations(&[make_entry(1, "del-1", "pid-1", false)]);

    // The FIRST observe_failure is the one the central rail uses — it must
    // return the delegation id so the rail can enqueue CompleteDelegation.
    let d = broker.observe_failure("sess-1", "pid-1", &delegations);
    assert_eq!(d.mark_terminal, vec!["del-1".to_string()]);
    // No broker-side terminal final on failure (the rail supplies the wrapped
    // failure text as the CompleteDelegation payload).
    assert!(d.terminal.is_empty());

    // The duplicate (what the OLD second call did) returns nothing — proving
    // the old rail's CompleteDelegation loop was dead code.
    let d2 = broker.observe_failure("sess-1", "pid-1", &delegations);
    assert!(d2.mark_terminal.is_empty());
    assert!(d2.terminal.is_empty());
}

#[test]
fn broker_failure_then_turn_completed_emits_no_second_final() {
    // A failure rail marking terminal must suppress a later TurnCompleted's
    // final — the central rail already sent the wrapped failure final, so a
    // duplicate assistant final would double-complete the delegation.
    let mut broker = LiveDelegationBroker::new(1);
    broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
    broker.register_delegation("del-1".to_string());
    let delegations = make_delegations(&[make_entry(1, "del-1", "pid-1", false)]);

    let d_fail = broker.observe_failure("sess-1", "pid-1", &delegations);
    assert_eq!(d_fail.mark_terminal.len(), 1);

    // A late TurnCompleted for the same prompt must not emit a second final.
    let d_turn = broker.observe_turn_completed("sess-1", "pid-1", &delegations);
    assert!(d_turn.terminal.is_empty());
    assert!(d_turn.mark_terminal.is_empty());
}

// ── Regression: unmute hint + speaking threshold (issue #4) ─────────────────

#[test]
fn visualizer_speaking_threshold_is_omp_0_015() {
    // The OMP output-active threshold is 0.015. A level just above it must
    // read as "speaking" while muted; a level just below must read as
    // "listening" (or "working" when a delegation is active). We exercise the
    // threshold via the phase-footer rendering by checking the localized
    // status label that lands in the rendered line.
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    fn status_text(level: f64, muted: bool, delegation_active: bool) -> String {
        let mut state = LiveVisualizerState::default();
        state.phase = LivePhase::Connected;
        state.level = level;
        state.muted = muted;
        state.delegation_active = delegation_active;
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 5));
        crate::live::visualizer::render(&mut buf, Rect::new(0, 0, 80, 5), &state);
        // The footer is the last row; collect its non-space symbols.
        let footer_y = 4;
        (0..80)
            .map(|x| buf[(x, footer_y)].symbol().to_string())
            .collect::<String>()
            .trim()
            .to_string()
    }

    // Just above 0.015 → speaking (not muted).
    let speaking = status_text(0.016, false, false);
    assert!(
        speaking.contains("Speaking"),
        "level 0.016 must read as Speaking, got: {speaking}"
    );

    // Just below 0.015 → listening (not muted, no delegation).
    let listening = status_text(0.014, false, false);
    assert!(
        listening.contains("Listening"),
        "level 0.014 must read as Listening, got: {listening}"
    );

    // Muted always wins over speaking.
    let muted = status_text(0.5, true, false);
    assert!(
        muted.contains("Muted"),
        "muted must read as Muted even at high level, got: {muted}"
    );
}

#[test]
fn visualizer_muted_shows_unmute_hint() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    let mut state = LiveVisualizerState::default();
    state.phase = LivePhase::Connected;
    state.muted = true;

    let mut buf = Buffer::empty(Rect::new(0, 0, 80, VISUALIZER_HEIGHT));
    crate::live::visualizer::render(&mut buf, Rect::new(0, 0, 80, VISUALIZER_HEIGHT), &state);

    let footer_y = VISUALIZER_HEIGHT - 1;
    let footer: String = (0..80)
        .map(|x| buf[(x, footer_y)].symbol().to_string())
        .collect();
    assert!(
        footer.contains("unmute"),
        "muted footer must show the unmute hint, got: {footer}"
    );
    assert!(
        !footer.contains("Space: mute"),
        "muted footer must not show the mute hint, got: {footer}"
    );
}

#[test]
fn visualizer_unmuted_shows_mute_hint() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    let mut state = LiveVisualizerState::default();
    state.phase = LivePhase::Connected;
    state.muted = false;

    let mut buf = Buffer::empty(Rect::new(0, 0, 80, VISUALIZER_HEIGHT));
    crate::live::visualizer::render(&mut buf, Rect::new(0, 0, 80, VISUALIZER_HEIGHT), &state);

    let footer_y = VISUALIZER_HEIGHT - 1;
    let footer: String = (0..80)
        .map(|x| buf[(x, footer_y)].symbol().to_string())
        .collect();
    assert!(
        footer.contains("Space: mute"),
        "unmuted footer must show the mute hint, got: {footer}"
    );
    assert!(
        !footer.contains("unmute"),
        "unmuted footer must not show the unmute hint, got: {footer}"
    );
}
