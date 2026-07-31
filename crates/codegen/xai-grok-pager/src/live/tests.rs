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
            .starts_with("\"Agent Final Message\":")
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
            final_message: "\"Agent Final Message\":\n\ndone".to_string(),
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
            assert!(text.starts_with("\"Agent Final Message\":"));
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
    let mut vis = LiveVisualizerState {
        phase: LivePhase::Connected,
        ..Default::default()
    };
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
fn visualizer_user_transcript_cumulative_resend_coalesces() {
    // OMP `#addTranscript`: input transcripts are cumulative re-sends. When
    // incoming `starts_with` the current, use incoming (the server re-sent the
    // accumulated text). This replaces the old space-join append that
    // duplicated cumulative input across deltas.
    let mut vis = LiveVisualizerState::default();
    vis.apply_event(&LiveEvent::Transcript {
        kind: TranscriptKind::Input,
        text: "Hello".to_string(),
    });
    assert_eq!(vis.user_transcript, "Hello");
    vis.apply_event(&LiveEvent::Transcript {
        kind: TranscriptKind::Input,
        text: "Hello world".to_string(),
    });
    assert_eq!(vis.user_transcript, "Hello world");
    // A trailing duplicate (current ends_with incoming) is kept as-is.
    vis.apply_event(&LiveEvent::Transcript {
        kind: TranscriptKind::Input,
        text: "world".to_string(),
    });
    assert_eq!(vis.user_transcript, "Hello world");
}

#[test]
fn visualizer_user_transcript_incremental_chunks_concatenate() {
    // OMP `#addTranscript`: when incoming is NOT a prefix-superset and NOT a
    // trailing duplicate, it is a new incremental chunk → concatenate (no
    // space, matching OMP which appends raw text).
    let mut vis = LiveVisualizerState::default();
    vis.apply_event(&LiveEvent::Transcript {
        kind: TranscriptKind::Input,
        text: "Hello".to_string(),
    });
    vis.apply_event(&LiveEvent::Transcript {
        kind: TranscriptKind::Input,
        text: " world".to_string(),
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
fn live_stop_message_preserves_terminal_cause_on_one_line() {
    assert_eq!(
        crate::live::handle::live_stop_message(Some(
            "Codex live sideband closed (1008):\n account policy changed"
        )),
        "Live stopped: Codex live sideband closed (1008): account policy changed"
    );
}

#[test]
fn live_stop_message_uses_generic_fallback_without_cause() {
    assert_eq!(
        crate::live::handle::live_stop_message(None),
        "Live stopped unexpectedly. Try again."
    );
    assert_eq!(
        crate::live::handle::live_stop_message(Some(" \n\t")),
        "Live stopped unexpectedly. Try again."
    );
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
    let area = Rect::new(0, 0, 80, 10);
    let mut buf = Buffer::empty(area);
    let state = LiveVisualizerState::default();
    let hits = crate::live::visualizer::render(&mut buf, area, &state);
    // Should have rendered something (not all empty), with both controls
    // clipped inside the footer row.
    assert!(buf.area().width > 0);
    assert!(
        hits.mute
            .is_some_and(|rect| area.contains((rect.x, rect.y).into()))
    );
    assert!(
        hits.stop
            .is_some_and(|rect| area.contains((rect.x, rect.y).into()))
    );
}

#[test]
fn visualizer_render_narrow_does_not_panic() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    let area = Rect::new(0, 0, 30, 10);
    let mut buf = Buffer::empty(area);
    let state = LiveVisualizerState::default();
    let hits = crate::live::visualizer::render(&mut buf, area, &state);
    assert!(buf.area().width > 0);
    assert!(hits.mute.is_some(), "narrow footer keeps the mute target");
    assert!(hits.stop.is_some(), "narrow footer keeps the stop target");
}

// ── Config tests ────────────────────────────────────────────────────────────

#[test]
fn live_config_built_with_omp_version() {
    let config = crate::live::config::build_live_config(
        "sess-1",
        "https://chatgpt.com/backend-api/codex",
        None,
        "sol",
    );
    assert_eq!(config.session_id, "sess-1");
    assert_eq!(config.codex_base, "https://chatgpt.com/backend-api/codex");
    assert_eq!(config.voice, "sol");
    assert_eq!(config.client_version, "0.144.1");
    assert!(config.sideband_base.is_none());
}

#[test]
fn live_default_codex_base_matches_platform_catalog() {
    let config = crate::live::config::build_live_config_default("sess-1");
    assert_eq!(
        config.codex_base,
        xai_grok_models::PlatformId::OpenAiCodex.base_url()
    );
}

#[test]
fn live_config_with_sideband() {
    let config = crate::live::config::build_live_config(
        "sess-1",
        "https://chatgpt.com/backend-api/codex",
        Some("wss://sideband".to_string()),
        "sol",
    );
    assert_eq!(config.sideband_base.as_deref(), Some("wss://sideband"));
}

#[test]
fn live_prompts_wrap_final_message() {
    let wrapped = crate::live::prompts::wrap_agent_final_message("All done");
    assert_eq!(wrapped, "\"Agent Final Message\":\n\nAll done");
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

    // The CompleteDelegation should be queued in pending_cmds (as a
    // PendingCritical with a stable sequence ID).
    assert_eq!(runtime.pending_cmds.len(), 1);
    assert!(matches!(
        &runtime.pending_cmds[0].cmd,
        LiveCommand::CompleteDelegation { delegation_id, .. } if delegation_id == "del-1"
    ));
    // Sequence IDs are monotonic starting at 1.
    assert_eq!(runtime.pending_cmds[0].seq, 1);

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
    assert!(matches!(runtime.pending_cmds[0].cmd, LiveCommand::Shutdown));
}

#[test]
fn command_queue_teardown_clears_pending() {
    use crate::live::LiveCommand;
    use crate::live::state::LiveRuntime;

    let mut runtime = LiveRuntime::default();
    runtime.send_cmd(LiveCommand::Shutdown);
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

// ── OMP exact transcript merge semantics (issue #2) ─────────────────────────
//
// The visualizer must implement OMP's `#addTranscript` / `#finishTranscript`
// merge rather than naive append/replace. These tests exercise the pure merge
// helpers plus the integrated `apply_event` path for both roles.

#[test]
fn omp_add_transcript_cumulative_chunks_use_incoming() {
    // `#addTranscript`: if incoming starts_with current, use incoming (the
    // server re-sent the accumulated text). No duplication.
    use crate::live::state::{RoleTurnState, merge_add_transcript};
    let mut turn = RoleTurnState::default();
    let a = merge_add_transcript("", "Hello", &mut turn);
    assert_eq!(a, "Hello");
    assert!(turn.active);
    // Cumulative resend: incoming starts_with current.
    let b = merge_add_transcript("Hello", "Hello world", &mut turn);
    assert_eq!(b, "Hello world");
    // Further cumulative resend.
    let c = merge_add_transcript("Hello world", "Hello world, done.", &mut turn);
    assert_eq!(c, "Hello world, done.");
}

#[test]
fn omp_add_transcript_incremental_chunks_concatenate() {
    // `#addTranscript`: if incoming is neither a prefix-superset nor a trailing
    // duplicate, it is a new incremental chunk → concatenate.
    use crate::live::state::{RoleTurnState, merge_add_transcript};
    let mut turn = RoleTurnState::default();
    let a = merge_add_transcript("", "Hello", &mut turn);
    assert_eq!(a, "Hello");
    // " world" does not start_with "Hello" and "Hello" does not end_with
    // " world" → concatenate.
    let b = merge_add_transcript("Hello", " world", &mut turn);
    assert_eq!(b, "Hello world");
}

#[test]
fn omp_add_transcript_suffix_duplicate_keeps_current() {
    // `#addTranscript`: if current ends_with incoming, keep current (incoming
    // is a trailing duplicate already present).
    use crate::live::state::{RoleTurnState, merge_add_transcript};
    let mut turn = RoleTurnState::default();
    let _ = merge_add_transcript("", "Hello world", &mut turn);
    // "world" is a trailing duplicate.
    let b = merge_add_transcript("Hello world", "world", &mut turn);
    assert_eq!(b, "Hello world");
}

#[test]
fn omp_add_transcript_starts_new_turn_after_final() {
    // `#addTranscript`: start a role-local turn if the current is empty OR
    // finalized. After a turn.done finalizes, the next delta starts fresh
    // (clears the prior turn's text) — the visualizer shows the CURRENT user
    // turn, not an accumulation across turns.
    let mut vis = LiveVisualizerState::default();
    // First user turn.
    vis.apply_event(&LiveEvent::Transcript {
        kind: TranscriptKind::Input,
        text: "first turn".to_string(),
    });
    vis.apply_event(&LiveEvent::Turn {
        role: LiveRole::User,
        transcript: "first turn".to_string(),
    });
    assert_eq!(vis.user_transcript, "first turn");
    assert!(!vis.user_turn.active);
    // Second user turn: the first delta starts a new turn (current was
    // finalized) → replaces, does not append to "first turn".
    vis.apply_event(&LiveEvent::Transcript {
        kind: TranscriptKind::Input,
        text: "second turn".to_string(),
    });
    assert_eq!(vis.user_transcript, "second turn");
    assert!(vis.user_turn.active);
}

#[test]
fn omp_add_transcript_ignores_late_equal_and_suffix_after_final() {
    use crate::live::state::{RoleTurnState, merge_add_transcript, merge_finish_transcript};

    let mut turn = RoleTurnState::default();
    let current = merge_finish_transcript("", "Hello world", &mut turn);
    assert_eq!(turn.finalized.as_deref(), Some("Hello world"));

    let equal = merge_add_transcript(&current, "Hello world", &mut turn);
    assert_eq!(equal, "Hello world");
    assert!(!turn.active);
    assert_eq!(turn.finalized.as_deref(), Some("Hello world"));

    let suffix = merge_add_transcript(&equal, "world", &mut turn);
    assert_eq!(suffix, "Hello world");
    assert!(!turn.active);
    assert_eq!(turn.finalized.as_deref(), Some("Hello world"));
}

#[test]
fn omp_add_transcript_different_delta_after_final_starts_new_turn() {
    use crate::live::state::{RoleTurnState, merge_add_transcript, merge_finish_transcript};

    let mut turn = RoleTurnState::default();
    let first = merge_finish_transcript("", "same words", &mut turn);
    let second = merge_add_transcript(&first, "new prelude", &mut turn);
    assert_eq!(second, "new prelude");
    assert!(turn.active);
    assert!(
        turn.finalized.is_none(),
        "new active turn clears old final marker"
    );

    // A new turn is allowed to finalize to text used by an older turn. It is
    // not mistaken for a duplicate because the intervening add cleared the
    // role-local finalized marker.
    let third = merge_finish_transcript(&second, "same words", &mut turn);
    assert_eq!(third, "same words");
    assert!(!turn.active);
    assert_eq!(turn.finalized.as_deref(), Some("same words"));
}

#[test]
fn omp_finish_transcript_preserves_longer_current_starting_with_final() {
    // `#finishTranscript`: preserve a longer current when it starts_with the
    // final text (the active incremental build may be ahead of the finalized
    // snapshot).
    use crate::live::state::{RoleTurnState, merge_finish_transcript};
    let mut turn = RoleTurnState {
        active: true,
        ..Default::default()
    };
    // The active current is longer and starts_with the final snapshot.
    let merged = merge_finish_transcript("Hello world, done.", "Hello world", &mut turn);
    assert_eq!(merged, "Hello world, done.");
    assert!(!turn.active);
    assert_eq!(turn.finalized.as_deref(), Some("Hello world"));
}

#[test]
fn omp_finish_transcript_dedup_repeated_final() {
    // `#finishTranscript`: dedup a repeated final — the finalized text is
    // unchanged, so the current is returned as-is and no re-flush is signaled.
    use crate::live::state::{RoleTurnState, merge_finish_transcript};
    let mut turn = RoleTurnState::default();
    let first = merge_finish_transcript("", "final text", &mut turn);
    assert_eq!(first, "final text");
    assert_eq!(turn.finalized.as_deref(), Some("final text"));
    // Repeated final — unchanged.
    let second = merge_finish_transcript("final text", "final text", &mut turn);
    assert_eq!(second, "final text");
    // The decision helper signals no change (the integrated path uses the
    // `changed` flag to skip re-flushing).
}

#[test]
fn omp_finish_transcript_different_final_after_final_starts_new_turn() {
    use crate::live::state::{RoleTurnState, merge_finish_transcript};

    let mut turn = RoleTurnState::default();
    let first = merge_finish_transcript("", "old finalized prefix", &mut turn);
    assert_eq!(first, "old finalized prefix");

    // Because the prior turn is already final, a different final establishes
    // another turn directly. It must not preserve the old current merely
    // because the old text happens to start with the new final.
    let second = merge_finish_transcript(&first, "old", &mut turn);
    assert_eq!(second, "old");
    assert_eq!(turn.finalized.as_deref(), Some("old"));
}

#[test]
fn omp_assistant_final_scrollback_contains_full_turn_exactly_once() {
    // The final assistant scrollback must contain the full merged/final turn
    // exactly once. A duplicate `turn.done` with the same final text must not
    // re-trigger a scrollback flush (the `assistant_flushed` flag is only
    // reset when the finalized text actually changes).
    let mut vis = LiveVisualizerState::default();
    // Streaming output deltas (cumulative).
    vis.apply_event(&LiveEvent::Transcript {
        kind: TranscriptKind::Output,
        text: "Hello".to_string(),
    });
    vis.apply_event(&LiveEvent::Transcript {
        kind: TranscriptKind::Output,
        text: "Hello world".to_string(),
    });
    // Final turn.done.
    let changed = vis.apply_event(&LiveEvent::Turn {
        role: LiveRole::Assistant,
        transcript: "Hello world".to_string(),
    });
    assert!(changed);
    assert!(!vis.assistant_flushed, "finalized turn needs a flush");
    assert_eq!(vis.assistant_transcript, "Hello world");
    // Simulate the scrollback flush.
    vis.assistant_flushed = true;

    // Duplicate turn.done with the same final text → no change, no re-flush.
    let changed2 = vis.apply_event(&LiveEvent::Turn {
        role: LiveRole::Assistant,
        transcript: "Hello world".to_string(),
    });
    assert!(!changed2, "duplicate final must not signal a change");
    assert!(
        vis.assistant_flushed,
        "duplicate final must not reset the flush flag"
    );
    assert_eq!(vis.assistant_transcript, "Hello world");
}

#[test]
fn omp_same_text_final_in_a_new_assistant_turn_flushes_again() {
    let mut vis = LiveVisualizerState::default();
    assert!(vis.apply_event(&LiveEvent::Turn {
        role: LiveRole::Assistant,
        transcript: "same final".to_string(),
    }));
    vis.assistant_flushed = true;

    // A genuinely new active turn clears the role-local final marker.
    assert!(vis.apply_event(&LiveEvent::Transcript {
        kind: TranscriptKind::Output,
        text: "different partial".to_string(),
    }));
    assert!(vis.assistant_turn.finalized.is_none());

    // It may finish with text identical to an older turn; that is a new final
    // and must re-arm the exactly-once scrollback flush.
    assert!(vis.apply_event(&LiveEvent::Turn {
        role: LiveRole::Assistant,
        transcript: "same final".to_string(),
    }));
    assert!(!vis.assistant_flushed);
    assert_eq!(vis.assistant_transcript, "same final");
}

#[test]
fn omp_finish_transcript_shorter_final_replaces_when_current_not_prefix() {
    // `#finishTranscript`: when the current does NOT start_with the final text,
    // the finalized text replaces the current (establish the final).
    use crate::live::state::{RoleTurnState, merge_finish_transcript};
    let mut turn = RoleTurnState {
        active: true,
        ..Default::default()
    };
    // Current "response" does not start_with "final response".
    let merged = merge_finish_transcript("response", "final response", &mut turn);
    assert_eq!(merged, "final response");
    assert!(!turn.active);
    assert_eq!(turn.finalized.as_deref(), Some("final response"));
}

// ── Regression: visualizer layout (issue #6) ────────────────────────────────

#[test]
fn visualizer_wide_renders_all_rows() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    let state = LiveVisualizerState {
        phase: LivePhase::Connected,
        level: 0.5,
        user_transcript: "test transcript".to_string(),
        ..Default::default()
    };

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

    let state = LiveVisualizerState {
        phase: LivePhase::Connected,
        level: 0.5,
        user_transcript: "narrow test".to_string(),
        muted: true,
        ..Default::default()
    };

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
// drain arm snapshots the head critical command (WITHOUT removing it), awaits
// the bounded channel's `send`, and — only on a confirmed successful send —
// forgets exactly that entry by its stable sequence ID. On timeout or
// cancellation the entry stays queued. This is cancellation-safe: a dropped
// pending `send` future can never lose a final command.

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
        text: "\"Agent Final Message\":\n\ndone".to_string(),
    });
    assert!(runtime.has_pending_critical());
    assert_eq!(runtime.pending_cmds.len(), 1);

    // The synchronous reaper can't deliver yet (channel still full).
    assert_eq!(runtime.drain_pending_cmds(), 0);
    assert!(runtime.has_pending_critical());

    // The async drain arm SNAPSHOTS the head critical command (does NOT
    // remove it) and awaits `send().await`. No unrelated app event fires
    // here — the wake must come from capacity, not an app event.
    let (seq, cmd, tx_clone) = runtime
        .snapshot_pending_critical_head()
        .expect("pending critical command present");
    // Snapshot did NOT remove the entry — it stays queued until the send is
    // confirmed (cancellation-safe).
    assert!(runtime.has_pending_critical());
    assert_eq!(runtime.pending_cmds.len(), 1);

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

    // Confirmed successful send → forget exactly this entry by sequence ID.
    assert!(runtime.forget_pending_critical(seq));
    // Nothing remains pending (the entry was removed exactly once).
    assert!(!runtime.has_pending_critical());
    assert!(runtime.pending_cmds.is_empty());
    // Forgetting again is a no-op (never sent twice).
    assert!(!runtime.forget_pending_critical(seq));

    // And the receiver observes the final.
    let received = rx.recv().await.expect("final received");
    match received {
        LiveCommand::CompleteDelegation {
            delegation_id,
            text,
        } => {
            assert_eq!(delegation_id, "del-final");
            assert!(text.starts_with("\"Agent Final Message\":"));
        }
        other => panic!("expected CompleteDelegation, got {other:?}"),
    }
}

#[tokio::test]
async fn critical_drain_cancellation_leaves_command_queued() {
    // Cancellation-safety: if a higher-priority select arm wins while the
    // drain arm's `send().await` is pending, the future is dropped but the
    // snapshotted entry MUST stay queued (it was never removed). The final
    // command is never lost.
    use crate::live::state::LiveRuntime;

    let mut runtime = LiveRuntime::default();
    // 1-slot channel — fill it and keep it full so the send never completes.
    let (tx, _rx) = tokio::sync::mpsc::channel::<LiveCommand>(1);
    runtime.cmd_tx = Some(tx);
    runtime
        .cmd_tx
        .as_ref()
        .unwrap()
        .try_send(LiveCommand::ToggleMute)
        .unwrap();

    // Enqueue a critical command, then snapshot it (as the arm does).
    runtime.send_cmd(LiveCommand::CompleteDelegation {
        delegation_id: "del-cancel".to_string(),
        text: "\"Agent Final Message\":\n\ncancelled".to_string(),
    });
    let (seq, cmd, tx_clone) = runtime
        .snapshot_pending_critical_head()
        .expect("pending critical command present");
    // The entry is STILL queued (snapshot doesn't remove).
    assert!(runtime.has_pending_critical());
    assert_eq!(runtime.pending_cmds.len(), 1);
    assert_eq!(runtime.pending_cmds[0].seq, seq);

    // Arm a send that will be CANCELLED before it completes (the channel
    // stays full, so `send` would block forever; we cancel it).
    let send_task = tokio::spawn(async move {
        // This blocks forever (channel stays full); the task is aborted below.
        tx_clone.send(cmd).await
    });
    // Yield once so the task arms its send.
    tokio::task::yield_now().await;
    // Cancel the send (simulating a higher-priority select arm winning).
    send_task.abort();
    let _ = send_task.await;

    // The entry MUST still be queued — cancellation never removed it.
    assert!(runtime.has_pending_critical());
    assert_eq!(runtime.pending_cmds.len(), 1);
    assert_eq!(runtime.pending_cmds[0].seq, seq);
    // We never called forget, so the command is intact.
    assert!(matches!(
        &runtime.pending_cmds[0].cmd,
        LiveCommand::CompleteDelegation { delegation_id, .. }
            if delegation_id == "del-cancel"
    ));
}

#[tokio::test]
async fn critical_drain_timeout_keeps_command_queued() {
    use crate::live::state::LiveRuntime;
    use std::time::Duration;
    use tokio::time::timeout;

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

    // Enqueue a critical command, then snapshot it (as the arm does).
    runtime.send_cmd(LiveCommand::CompleteDelegation {
        delegation_id: "del-to".to_string(),
        text: "\"Agent Final Message\":\n\ntimeout".to_string(),
    });
    let (seq, cmd, tx_clone) = runtime
        .snapshot_pending_critical_head()
        .expect("pending critical command present");
    // Snapshot didn't remove the entry.
    assert_eq!(runtime.pending_cmds.len(), 1);

    // Simulate a timed-out send: the send times out (channel stays full), so
    // we do NOT call forget — the entry stays queued for the next iteration.
    let send_result = timeout(Duration::from_millis(10), tx_clone.send(cmd)).await;
    assert!(send_result.is_err(), "send must time out (channel full)");
    // On timeout, the arm does NOT forget — the entry stays queued.
    assert!(runtime.has_pending_critical());
    assert_eq!(runtime.pending_cmds.len(), 1);
    assert_eq!(runtime.pending_cmds[0].seq, seq);
    // It must be retried in order (still the head).
    assert!(matches!(
        &runtime.pending_cmds[0].cmd,
        LiveCommand::CompleteDelegation { delegation_id, .. }
            if delegation_id == "del-to"
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
        let state = LiveVisualizerState {
            phase: LivePhase::Connected,
            level,
            muted,
            delegation_active,
            ..Default::default()
        };
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

    let state = LiveVisualizerState {
        phase: LivePhase::Connected,
        muted: true,
        ..Default::default()
    };

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

    let state = LiveVisualizerState {
        phase: LivePhase::Connected,
        muted: false,
        ..Default::default()
    };

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
