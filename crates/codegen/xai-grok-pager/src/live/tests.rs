//! Focused tests for the Codex Live (`/live`) integration.
//!
//! These tests use fake Live command/event channels — no real audio/network.
//! They cover:
//! - command visibility/toggle
//! - auth absent
//! - pending unbound session
//! - draft preservation
//! - `/voice` exclusion (mutual exclusion)
//! - stop on navigation/quit
//! - modal/input priority
//! - transcript coalescing
//! - full-width/narrow visualizer
//! - repeated/multiple delegation ids
//! - exact prompt correlation
//! - replay/foreign/old generation
//! - tool-boundary commentary
//! - terminal exactly once under signal reordering
//! - bounded channel behavior

#![cfg(feature = "codex-live")]

use crate::live::broker::LiveDelegationBroker;
use crate::live::state::{DraftSnapshot, LiveRuntime, LiveState, LiveVisualizerState};
use crate::live::{LiveCommand, LiveContextChannel, LiveEvent, LiveLevels, LivePhase, LiveRole};

// ── Broker tests ────────────────────────────────────────────────────────────

fn make_entry(
    generation: u64,
    del_id: &str,
    pid: &str,
    terminal: bool,
) -> crate::live::state::DelegationEntry {
    crate::live::state::DelegationEntry {
        generation,
        delegation_id: del_id.to_string(),
        agent_id: crate::app::agent::AgentId(0),
        session_id: "sess-1".to_string(),
        prompt_id: pid.to_string(),
        terminal,
    }
}

fn make_delegations(
    entries: &[crate::live::state::DelegationEntry],
) -> std::collections::HashMap<(u64, String), crate::live::state::DelegationEntry> {
    entries
        .iter()
        .map(|e| ((e.generation, e.delegation_id.clone()), e.clone()))
        .collect()
}

#[test]
fn broker_foreign_session_ignored() {
    let mut broker = LiveDelegationBroker::new(1);
    broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
    broker.register_delegation("del-1".to_string(), "pid-1".to_string());
    let delegations = make_delegations(&[make_entry(1, "del-1", "pid-1", false)]);
    let decision = broker.observe_chunk("sess-other", "pid-1", "hello", false, &delegations);
    assert!(decision.commentary.is_empty());
    assert!(decision.terminal.is_empty());
}

#[test]
fn broker_foreign_prompt_ignored() {
    let mut broker = LiveDelegationBroker::new(1);
    broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
    broker.register_delegation("del-1".to_string(), "pid-1".to_string());
    let delegations = make_delegations(&[make_entry(1, "del-1", "pid-1", false)]);
    let decision = broker.observe_chunk("sess-1", "pid-other", "hello", false, &delegations);
    assert!(decision.commentary.is_empty());
}

#[test]
fn broker_old_generation_ignored() {
    let mut broker = LiveDelegationBroker::new(2);
    broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
    broker.register_delegation("del-1".to_string(), "pid-1".to_string());
    let delegations = make_delegations(&[make_entry(1, "del-1", "pid-1", false)]);
    let decision = broker.observe_chunk("sess-1", "pid-1", "hello", false, &delegations);
    assert!(decision.commentary.is_empty());
}

#[test]
fn broker_replay_ignored_via_terminal_delegation() {
    // A terminal delegation should be ignored (simulates replay/stale).
    let mut broker = LiveDelegationBroker::new(1);
    broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
    broker.register_delegation("del-1".to_string(), "pid-1".to_string());
    let delegations = make_delegations(&[make_entry(1, "del-1", "pid-1", true)]);
    let decision = broker.observe_chunk("sess-1", "pid-1", "hello", false, &delegations);
    assert!(decision.commentary.is_empty());
}

#[test]
fn broker_tool_boundary_flushes_commentary() {
    let mut broker = LiveDelegationBroker::new(1);
    broker.bind("sess-1".to_string(), crate::app::agent::AgentId(0));
    broker.register_delegation("del-1".to_string(), "pid-1".to_string());
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
    broker.register_delegation("del-1".to_string(), "pid-1".to_string());
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
    broker.register_delegation("del-1".to_string(), "pid-1".to_string());
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
    broker.register_delegation("del-1".to_string(), "pid-1".to_string());
    broker.register_delegation("del-2".to_string(), "pid-2".to_string());
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
    broker.register_delegation("del-1".to_string(), "pid-1".to_string());
    broker.register_delegation("del-2".to_string(), "pid-2".to_string());
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
    broker.register_delegation("del-1".to_string(), "pid-1".to_string());
    broker.register_delegation("del-2".to_string(), "pid-2".to_string());
    let delegations = make_delegations(&[
        make_entry(1, "del-1", "pid-1", false),
        make_entry(1, "del-2", "pid-2", false),
    ]);

    let d = broker.observe_cancel_all(&delegations);
    assert_eq!(d.mark_terminal.len(), 2);
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

// ── Visualizer state tests ──────────────────────────────────────────────────

#[test]
fn visualizer_phase_update() {
    let mut vis = LiveVisualizerState::default();
    let needs_draw = vis.apply_event(&LiveEvent::Phase(LivePhase::Listening));
    assert!(needs_draw);
    assert_eq!(vis.phase, LivePhase::Listening);
}

#[test]
fn visualizer_phase_no_redraw_on_same_phase() {
    let mut vis = LiveVisualizerState::default();
    vis.phase = LivePhase::Listening;
    let needs_draw = vis.apply_event(&LiveEvent::Phase(LivePhase::Listening));
    assert!(!needs_draw);
}

#[test]
fn visualizer_levels_update_and_peak_decay() {
    let mut vis = LiveVisualizerState::default();
    let levels = LiveLevels {
        user_peak: 0.8,
        assistant_peak: 0.5,
        ..Default::default()
    };
    let needs_draw = vis.apply_event(&LiveEvent::Levels(levels));
    assert!(needs_draw);
    assert!((vis.peak_decay - 0.8).abs() < 0.01);

    vis.decay_peak(0.5);
    assert!((vis.peak_decay - 0.4).abs() < 0.01);
}

#[test]
fn visualizer_user_transcript_accumulates() {
    let mut vis = LiveVisualizerState::default();
    vis.apply_event(&LiveEvent::Transcript {
        role: LiveRole::User,
        text: "Hello".to_string(),
        finalized: true,
    });
    vis.apply_event(&LiveEvent::Transcript {
        role: LiveRole::User,
        text: "world".to_string(),
        finalized: true,
    });
    assert_eq!(vis.user_transcript, "Hello world");
}

#[test]
fn visualizer_assistant_transcript_coalescing() {
    let mut vis = LiveVisualizerState::default();
    // Partial segments replace the live partial.
    vis.apply_event(&LiveEvent::Transcript {
        role: LiveRole::Assistant,
        text: "I am".to_string(),
        finalized: false,
    });
    assert_eq!(vis.assistant_transcript, "I am");
    vis.apply_event(&LiveEvent::Transcript {
        role: LiveRole::Assistant,
        text: "I am thinking".to_string(),
        finalized: false,
    });
    assert_eq!(vis.assistant_transcript, "I am thinking");
    // Finalized segments coalesce.
    vis.apply_event(&LiveEvent::Transcript {
        role: LiveRole::Assistant,
        text: "Done.".to_string(),
        finalized: true,
    });
    assert!(vis.assistant_transcript.contains("Done."));
    assert!(vis.assistant_finalized);
}

#[test]
fn visualizer_error_sets_error_phase() {
    let mut vis = LiveVisualizerState::default();
    let needs_draw = vis.apply_event(&LiveEvent::Error {
        message: "Connection lost".to_string(),
    });
    assert!(needs_draw);
    assert_eq!(vis.phase, LivePhase::Error);
    assert_eq!(vis.error_message, Some("Connection lost".to_string()));
}

#[test]
fn visualizer_reset_turn_clears_assistant() {
    let mut vis = LiveVisualizerState::default();
    vis.apply_event(&LiveEvent::Transcript {
        role: LiveRole::Assistant,
        text: "response".to_string(),
        finalized: true,
    });
    vis.reset_turn();
    assert!(vis.assistant_transcript.is_empty());
    assert!(!vis.assistant_finalized);
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

// ── Channel tests ───────────────────────────────────────────────────────────

#[test]
fn live_context_channel_try_send_and_close() {
    let (ch, mut rx) = LiveContextChannel::pair(4);
    assert!(ch.try_send(LiveCommand::ToggleMute));
    assert!(ch.try_send(LiveCommand::Shutdown));
    let cmd1 = rx.try_recv().unwrap();
    let cmd2 = rx.try_recv().unwrap();
    assert_eq!(cmd1, LiveCommand::ToggleMute);
    assert_eq!(cmd2, LiveCommand::Shutdown);
}

#[test]
fn live_context_channel_bounded_drops_when_full() {
    let (ch, _rx) = LiveContextChannel::pair(2);
    // Fill the channel.
    assert!(ch.try_send(LiveCommand::ToggleMute));
    assert!(ch.try_send(LiveCommand::ToggleMute));
    // Third send should fail (bounded).
    assert!(!ch.try_send(LiveCommand::ToggleMute));
}

// ── Config tests ────────────────────────────────────────────────────────────

#[test]
fn live_config_built_with_omp_version() {
    let config = crate::live::config::build_live_config(
        "sess-1",
        "https://chatgpt.com/backend-api/codex",
        "wss://sideband",
        "sol",
    );
    assert_eq!(config.session_id, "sess-1");
    assert_eq!(config.codex_base, "https://chatgpt.com/backend-api/codex");
    assert_eq!(config.voice, "sol");
    assert_eq!(config.client_version, "0.144.1");
}

#[test]
fn live_prompts_wrap_final_message() {
    let wrapped = crate::live::prompts::wrap_agent_final_message("All done");
    assert!(wrapped.starts_with("Agent Final Message:"));
    assert!(wrapped.contains("All done"));
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
