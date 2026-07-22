//! Unit tests for the turn-finalize rails in [`super`] (`turn_completion`),
//! split out via `#[path]` to keep the module itself small.

use super::*;
use crate::app::agent::AgentState;
use crate::scrollback::block::RenderBlock;
use crate::scrollback::blocks::SessionEventBlock;
use crate::scrollback::state::ScrollbackState;
use std::path::PathBuf;
use std::time::Instant;

fn last_session_event(sb: &ScrollbackState) -> Option<SessionEvent> {
    (0..sb.len())
        .rev()
        .find_map(|i| match sb.get(i).map(|e| &e.block) {
            Some(RenderBlock::SessionEvent(b)) => Some(b.event.clone()),
            _ => None,
        })
}

/// A viewer in TurnRunning with an adopted prompt id, ready to be finalized.
fn running_viewer(prompt_id: &str) -> AgentView {
    let mut agent = super::super::agent_view::test_agent_view(Some("s1"), PathBuf::from("/tmp"));
    agent.attached_as_viewer = true;
    agent.session.start_turn(&mut agent.scrollback);
    agent.session.current_prompt_id = Some(prompt_id.into());
    agent.turn_started_at = Some(Instant::now());
    agent
}

/// A driver in TurnRunning with a local prompt id (default
/// `attached_as_viewer == false`).
fn running_driver(prompt_id: &str) -> AgentView {
    let mut agent = super::super::agent_view::test_agent_view(Some("s1"), PathBuf::from("/tmp"));
    agent.session.start_turn(&mut agent.scrollback);
    agent.session.current_prompt_id = Some(prompt_id.into());
    agent.turn_started_at = Some(Instant::now());
    agent
}

#[test]
fn viewer_finalize_idles_and_pushes_completed_marker() {
    let mut agent = running_viewer("p1");
    let outcome =
        finalize_turn_from_terminal(&mut agent, "s1", Some("p1"), Some("end_turn"), None, None);
    assert!(matches!(outcome, TerminalApply::ViewerFinalized));
    assert!(agent.session.state.is_idle());
    assert!(agent.session.current_prompt_id.is_none());
    assert!(agent.turn_started_at.is_none());
    assert!(matches!(
        last_session_event(&agent.scrollback),
        Some(SessionEvent::TurnCompleted { .. })
    ));
}

fn one_stop_group() -> Vec<(String, Vec<crate::scrollback::blocks::tool::HookRunEntry>)> {
    use crate::scrollback::blocks::tool::{HookRunEntry, HookRunStatus};
    vec![(
        "stop".to_string(),
        vec![HookRunEntry {
            name: "global/notify".into(),
            status: HookRunStatus::Success {
                elapsed: std::time::Duration::from_millis(12),
            },
            output: None,
        }],
    )]
}

/// Stop-hook groups attached to the last session-event marker.
fn last_marker_groups(sb: &ScrollbackState) -> Option<usize> {
    (0..sb.len())
        .rev()
        .find_map(|i| match sb.get(i).map(|e| &e.block) {
            Some(RenderBlock::SessionEvent(b)) => Some(b.stop_hooks.len()),
            _ => None,
        })
}

fn count_lifecycle_blocks(sb: &ScrollbackState) -> usize {
    use crate::scrollback::blocks::tool::ToolCallBlock;
    (0..sb.len())
        .filter(|i| {
            matches!(
                sb.get(*i).map(|e| &e.block),
                Some(RenderBlock::ToolCall(ToolCallBlock::Lifecycle(_)))
            )
        })
        .count()
}

#[test]
fn marker_push_consumes_matching_stop_hook_stash() {
    let mut agent = running_driver("p1");
    agent.pending_stop_hooks = Some(super::super::agent_view::PendingStopHooks {
        prompt_id: Some("p1".into()),
        groups: one_stop_group(),
    });

    push_turn_terminal_marker(
        &mut agent,
        Some(SessionEvent::TurnCompleted {
            elapsed: Some(std::time::Duration::from_secs(2)),
        }),
        Some("p1"),
    );

    assert_eq!(
        last_marker_groups(&agent.scrollback),
        Some(1),
        "the stash must fold into the marker"
    );
    assert!(agent.pending_stop_hooks.is_none());
    assert_eq!(count_lifecycle_blocks(&agent.scrollback), 0);
}

#[test]
fn marker_push_flushes_stale_stash_standalone() {
    // A stash stamped with another turn's prompt id must not attach to
    // this marker — it flushes as the legacy standalone block.
    let mut agent = running_driver("p2");
    agent.pending_stop_hooks = Some(super::super::agent_view::PendingStopHooks {
        prompt_id: Some("p1".into()),
        groups: one_stop_group(),
    });

    push_turn_terminal_marker(
        &mut agent,
        Some(SessionEvent::TurnCompleted {
            elapsed: Some(std::time::Duration::from_secs(2)),
        }),
        Some("p2"),
    );

    assert_eq!(
        last_marker_groups(&agent.scrollback),
        Some(0),
        "a stale stash must not attach to the new marker"
    );
    assert_eq!(count_lifecycle_blocks(&agent.scrollback), 1);
    assert!(agent.pending_stop_hooks.is_none());
}

#[test]
fn marker_without_ending_pid_flushes_stamped_stash_standalone() {
    // A stamped stash can't be confirmed against a marker whose ending
    // turn id is missing — it flushes standalone instead of folding into
    // a marker it may not belong to.
    let mut agent = running_driver("p1");
    agent.pending_stop_hooks = Some(super::super::agent_view::PendingStopHooks {
        prompt_id: Some("p1".into()),
        groups: one_stop_group(),
    });

    push_turn_terminal_marker(
        &mut agent,
        Some(SessionEvent::TurnCompleted {
            elapsed: Some(std::time::Duration::from_secs(2)),
        }),
        None,
    );

    assert_eq!(
        last_marker_groups(&agent.scrollback),
        Some(0),
        "an unconfirmable stamped stash must not attach to the marker"
    );
    assert_eq!(count_lifecycle_blocks(&agent.scrollback), 1);
    assert!(agent.pending_stop_hooks.is_none());
}

#[test]
fn no_marker_flushes_stash_as_standalone_block() {
    // Turn ends without a marker (bash turn / rate-limit UX): the held
    // hooks still surface, in the legacy standalone form.
    let mut agent = running_driver("p1");
    agent.pending_stop_hooks = Some(super::super::agent_view::PendingStopHooks {
        prompt_id: Some("p1".into()),
        groups: one_stop_group(),
    });

    push_turn_terminal_marker(&mut agent, None, Some("p1"));

    assert_eq!(count_lifecycle_blocks(&agent.scrollback), 1);
    assert!(agent.pending_stop_hooks.is_none());
}

#[test]
fn viewer_finalize_consumes_stop_hook_stash() {
    // A viewer that stashed hooks mid-turn folds them into the marker the
    // finalize pushes.
    let mut agent = running_viewer("p1");
    agent.pending_stop_hooks = Some(super::super::agent_view::PendingStopHooks {
        prompt_id: Some("p1".into()),
        groups: one_stop_group(),
    });

    let _ = finalize_turn_from_terminal(&mut agent, "s1", Some("p1"), Some("end_turn"), None, None);

    assert_eq!(last_marker_groups(&agent.scrollback), Some(1));
    assert!(agent.pending_stop_hooks.is_none());
}

#[test]
fn viewer_finalize_duplicate_terminal_is_noop() {
    let mut agent = running_viewer("p1");
    let _ = finalize_turn_from_terminal(&mut agent, "s1", Some("p1"), Some("end_turn"), None, None);
    let len_after_first = agent.scrollback.len();

    // A duplicate/stale terminal for the now-finished turn does nothing.
    let outcome =
        finalize_turn_from_terminal(&mut agent, "s1", Some("p1"), Some("end_turn"), None, None);
    assert!(matches!(outcome, TerminalApply::Ignored));
    assert!(agent.session.state.is_idle());
    assert_eq!(
        agent.scrollback.len(),
        len_after_first,
        "a duplicate terminal must not push a second marker"
    );
}

#[test]
fn viewer_finalize_stop_reason_to_marker_mapping() {
    // cancelled → Turn cancelled.
    let mut agent = running_viewer("p1");
    let _ =
        finalize_turn_from_terminal(&mut agent, "s1", Some("p1"), Some("cancelled"), None, None);
    assert!(matches!(
        last_session_event(&agent.scrollback),
        Some(SessionEvent::TurnCancelled { .. })
    ));

    // error (+agentResult) → Turn failed carrying the error text.
    let mut agent = running_viewer("p1");
    let _ = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        Some("p1"),
        Some("error"),
        Some("boom"),
        None,
    );
    match last_session_event(&agent.scrollback) {
        Some(SessionEvent::TurnFailed { error, .. }) => assert_eq!(error, "boom"),
        other => panic!("expected TurnFailed, got {other:?}"),
    }

    // rate_limit → finished, but no marker (not actionable from a viewer).
    let mut agent = running_viewer("p1");
    let _ =
        finalize_turn_from_terminal(&mut agent, "s1", Some("p1"), Some("rate_limit"), None, None);
    assert!(agent.session.state.is_idle());
    assert!(
        last_session_event(&agent.scrollback).is_none(),
        "rate_limit must not push a marker on a viewer"
    );

    // unknown/other reason → Turn completed (the catch-all).
    let mut agent = running_viewer("p1");
    let _ =
        finalize_turn_from_terminal(&mut agent, "s1", Some("p1"), Some("max_tokens"), None, None);
    assert!(matches!(
        last_session_event(&agent.scrollback),
        Some(SessionEvent::TurnCompleted { .. })
    ));
}

#[test]
fn driver_arms_reconcile_and_does_not_finish() {
    let mut agent = running_driver("p1");
    let outcome =
        finalize_turn_from_terminal(&mut agent, "s1", Some("p1"), Some("cancelled"), None, None);
    assert!(matches!(outcome, TerminalApply::ReconcileArmed));
    assert!(
        matches!(agent.session.state, AgentState::TurnRunning),
        "the driver's turn must NOT be finished — the PromptResponse RPC owns it"
    );
    let pending = agent
        .pending_turn_end_reconcile
        .as_ref()
        .expect("the driver's awaited turn must arm a reconcile");
    assert_eq!(pending.prompt_id, "p1");
    assert_eq!(pending.stop_reason.as_deref(), Some("cancelled"));
}

#[test]
fn driver_mismatched_prompt_id_does_not_arm() {
    // Stale/peer terminal must not arm reconcile on a different live turn.
    let mut agent = running_driver("p1");
    let outcome = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        Some("p-other"),
        Some("end_turn"),
        None,
        None,
    );
    assert!(matches!(outcome, TerminalApply::Ignored));
    assert!(agent.pending_turn_end_reconcile.is_none());
    assert!(matches!(agent.session.state, AgentState::TurnRunning));
}

#[test]
fn driver_missing_prompt_id_arms_against_current_when_idle_in_turn() {
    let mut agent = running_driver("p1");
    let outcome = finalize_turn_from_terminal(&mut agent, "s1", None, Some("end_turn"), None, None);
    assert!(matches!(outcome, TerminalApply::ReconcileArmed));
    assert_eq!(
        agent.pending_turn_end_reconcile.as_ref().unwrap().prompt_id,
        "p1"
    );
}

fn stream_agent_text(agent: &mut AgentView, text: &str) {
    use crate::acp::meta::NotificationMeta;
    use agent_client_protocol as acp;
    let meta = NotificationMeta::default();
    let _ = agent.session.tracker.handle_update(
        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
            acp::TextContent::new(text),
        ))),
        &meta,
        &mut agent.scrollback,
    );
}

/// Missing wire pid still arms lost-PR reconcile (full teardown
/// lives in `reconcile_overdue_turn_ends` / PromptResponse).
#[test]
fn repro_terminal_without_prompt_id_arms_reconcile_for_lost_pr() {
    let mut agent = running_driver("p1");
    stream_agent_text(&mut agent, "done");
    let outcome = finalize_turn_from_terminal(&mut agent, "s1", None, Some("end_turn"), None, None);
    assert!(matches!(outcome, TerminalApply::ReconcileArmed));
    assert!(agent.pending_turn_end_reconcile.is_some());
    assert!(
        matches!(agent.session.state, AgentState::TurnRunning),
        "driver stays TurnRunning until PR or overdue reconcile"
    );
    assert_eq!(
        agent.session.tracker.activity(),
        Some(crate::acp::tracker::TurnActivity::Responding)
    );
}

/// Exact pid still arms (control).
#[test]
fn recovery_mode_matching_turn_completed_arms_reconcile_for_lost_pr() {
    let mut agent = running_driver("p1");
    stream_agent_text(&mut agent, "done");

    let outcome =
        finalize_turn_from_terminal(&mut agent, "s1", Some("p1"), Some("end_turn"), None, None);
    assert!(matches!(outcome, TerminalApply::ReconcileArmed));
    assert!(matches!(agent.session.state, AgentState::TurnRunning));
    assert!(agent.pending_turn_end_reconcile.is_some());
}

/// Re-arm same pid keeps earliest received_at (does not extend grace forever).
#[test]
fn driver_rearm_same_pid_preserves_received_at() {
    let mut agent = running_driver("p1");
    let _ = finalize_turn_from_terminal(&mut agent, "s1", Some("p1"), Some("end_turn"), None, None);
    let first = agent
        .pending_turn_end_reconcile
        .as_ref()
        .unwrap()
        .received_at;
    std::thread::sleep(std::time::Duration::from_millis(5));
    let _ = finalize_turn_from_terminal(&mut agent, "s1", Some("p1"), Some("end_turn"), None, None);
    let second = agent
        .pending_turn_end_reconcile
        .as_ref()
        .unwrap()
        .received_at;
    assert_eq!(first, second);
}

// ── End markers: always the plain event text (work lives in the status row) ──

fn insert_bg_task(agent: &mut AgentView, task_id: &str, is_monitor: bool) {
    agent.session.bg_tasks.insert(
        task_id.into(),
        crate::app::agent::BgTaskState {
            task_id: task_id.into(),
            tool_call_id: format!("call-{task_id}"),
            command: "sleep 5".into(),
            description: None,
            cwd: "/tmp".into(),
            output_file: "/tmp/out".into(),
            status: crate::app::agent::BgTaskStatus::Running,
            start_time: std::time::SystemTime::now(),
            end_time: None,
            exit_code: None,
            signal: None,
            stdout: String::new(),
            stdout_line_count: 0,
            truncated: false,
            pending_kill: false,
            kill_requested_at: None,
            scrollback_entry_id: None,
            is_monitor,
            restored_from_replay: false,
        },
    );
}

/// The newest session-event marker block.
fn last_marker_block(agent: &AgentView) -> &SessionEventBlock {
    (0..agent.scrollback.len())
        .rev()
        .find_map(|i| match agent.scrollback.get(i).map(|e| &e.block) {
            Some(RenderBlock::SessionEvent(b)) => Some(b),
            _ => None,
        })
        .expect("a session-event marker must exist")
}

#[test]
fn real_end_marker_stays_plain_with_running_work() {
    // Background work never rides the end marker as a "still running" suffix
    // — the persistent "… still running" status row carries it instead. The
    // running command shows up in the watchers count only.
    let mut agent = running_driver("p1");
    insert_bg_task(&mut agent, "bg-1", false);

    push_turn_terminal_marker(
        &mut agent,
        Some(SessionEvent::TurnCompleted {
            elapsed: Some(std::time::Duration::from_secs(2)),
        }),
        Some("p1"),
    );

    let block = last_marker_block(&agent);
    assert!(!block.parked);
    assert_eq!(block.prompt_id.as_deref(), Some("p1"));
    assert_eq!(block.event.message(), "Worked for 2.0s");
    assert_eq!(
        agent.watchers().commands,
        1,
        "the running command feeds the status-row watchers cue instead"
    );
}

#[test]
fn workless_marker_renders_legacy_text() {
    let mut agent = running_driver("p1");

    push_turn_terminal_marker(
        &mut agent,
        Some(SessionEvent::TurnCompleted {
            elapsed: Some(std::time::Duration::from_secs(2)),
        }),
        Some("p1"),
    );

    let block = last_marker_block(&agent);
    assert_eq!(block.event.message(), "Worked for 2.0s");
}

// ── Send-now cancel marker suppression (viewer finalize rail) ────────

/// A viewer finalizing a `send_now`-stamped `cancelled` terminal pushes no marker.
#[test]
fn viewer_finalize_suppresses_send_now_cancel_marker() {
    let mut agent = running_viewer("p1");
    let outcome = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        Some("p1"),
        Some("cancelled"),
        None,
        Some("send_now"),
    );
    assert!(matches!(outcome, TerminalApply::ViewerFinalized));
    assert!(agent.session.state.is_idle(), "the turn still finishes");
    assert!(
        last_session_event(&agent.scrollback).is_none(),
        "a send-now cancel pushes no marker on a viewer"
    );

    // A non-send-now trigger keeps the marker even with a local expectation armed (wire is authoritative).
    let mut agent = running_viewer("p1");
    agent.expect_send_now_cancel = Some("p-mine".into());
    let _ = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        Some("p1"),
        Some("cancelled"),
        None,
        Some("ctrl_c"),
    );
    assert!(matches!(
        last_session_event(&agent.scrollback),
        Some(SessionEvent::TurnCancelled { .. })
    ));

    // Older shell (no meta): the armed expectation is the fallback.
    let mut agent = running_viewer("p1");
    agent.expect_send_now_cancel = Some("p-mine".into());
    let _ =
        finalize_turn_from_terminal(&mut agent, "s1", Some("p1"), Some("cancelled"), None, None);
    assert!(
        last_session_event(&agent.scrollback).is_none(),
        "the armed expectation suppresses the marker without wire meta"
    );
    assert!(
        agent.expect_send_now_cancel.is_none(),
        "the expectation is consumed on the viewer finalize"
    );
}

/// The driver arm records the broadcast's `cancelTrigger` for a lost-RPC reconcile.
#[test]
fn driver_arm_records_cancel_trigger_for_reconcile() {
    let mut agent = running_driver("p1");
    let outcome = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        Some("p1"),
        Some("cancelled"),
        None,
        Some("send_now"),
    );
    assert!(matches!(outcome, TerminalApply::ReconcileArmed));
    assert_eq!(
        agent
            .pending_turn_end_reconcile
            .as_ref()
            .and_then(|p| p.cancel_trigger.as_deref()),
        Some("send_now")
    );
}

/// All `TurnCompleted` markers (parked and final) in scrollback order.
fn completed_markers(sb: &ScrollbackState) -> Vec<SessionEventBlock> {
    (0..sb.len())
        .filter_map(|i| match sb.get(i).map(|e| &e.block) {
            Some(RenderBlock::SessionEvent(b))
                if matches!(b.event, SessionEvent::TurnCompleted { .. }) =>
            {
                Some(b.clone())
            }
            _ => None,
        })
        .collect()
}

/// The tail parked marker `maybe_push_parked_marker` leaves mid-turn (same
/// shape as `push_parked_marker_block`).
fn push_parked_tail(agent: &mut AgentView, prompt_id: &str, secs: u64) {
    let mut parked = SessionEventBlock::new(SessionEvent::TurnCompleted {
        elapsed: Some(std::time::Duration::from_secs(secs)),
    });
    parked.parked = true;
    parked.prompt_id = Some(prompt_id.into());
    agent
        .scrollback
        .push_block(RenderBlock::SessionEvent(parked));
}

#[test]
fn completion_folds_tail_parked_marker_instead_of_duplicating() {
    // Park → work finished → turn ended with nothing in between: the
    // completion folds into the parked marker, not a second identical row.
    let mut agent = running_driver("p1");
    push_parked_tail(&mut agent, "p1", 3);

    push_turn_terminal_marker(
        &mut agent,
        Some(SessionEvent::TurnCompleted {
            elapsed: Some(std::time::Duration::from_secs(5)),
        }),
        Some("p1"),
    );

    let markers = completed_markers(&agent.scrollback);
    assert_eq!(
        markers.len(),
        1,
        "park + completion must render ONE marker, got {}",
        markers.len()
    );
    assert!(!markers[0].parked, "the folded marker is the real turn end");
    assert_eq!(
        markers[0].event.message(),
        "Worked for 5.0s",
        "the folded marker carries the final elapsed"
    );
}

#[test]
fn completion_fold_attaches_stop_hooks_to_folded_marker() {
    let mut agent = running_driver("p1");
    push_parked_tail(&mut agent, "p1", 3);
    agent.pending_stop_hooks = Some(super::super::agent_view::PendingStopHooks {
        prompt_id: Some("p1".into()),
        groups: one_stop_group(),
    });

    push_turn_terminal_marker(
        &mut agent,
        Some(SessionEvent::TurnCompleted {
            elapsed: Some(std::time::Duration::from_secs(5)),
        }),
        Some("p1"),
    );

    let markers = completed_markers(&agent.scrollback);
    assert_eq!(markers.len(), 1);
    assert_eq!(
        markers[0].stop_hooks.len(),
        1,
        "stop hooks must ride the folded marker"
    );
    // A hook-carrying marker rests Collapsed on the fresh-push and
    // attach_stop_hooks paths; the fold must match.
    let folded = agent.scrollback.last().expect("folded marker entry");
    assert_eq!(
        folded.display_mode,
        crate::scrollback::types::DisplayMode::Collapsed,
        "folded marker with stop hooks must rest collapsed"
    );
}

#[test]
fn completion_folds_marker_pushed_by_real_park_path() {
    // Drive the real park — blocking wait through the tracker, then
    // `maybe_push_parked_marker` — so the fold's keys (parked + prompt_id)
    // stay pinned to the production marker shape, not the test helper's.
    let mut agent = running_driver("p1");
    super::super::agent_view::test_fixtures::simulate_task_output_wait(&mut agent, "bg-1");
    agent.maybe_push_parked_marker();
    let parked = completed_markers(&agent.scrollback);
    assert_eq!(parked.len(), 1, "real park path must push one marker");
    assert!(parked[0].parked);

    push_turn_terminal_marker(
        &mut agent,
        Some(SessionEvent::TurnCompleted {
            elapsed: Some(std::time::Duration::from_secs(5)),
        }),
        Some("p1"),
    );

    let markers = completed_markers(&agent.scrollback);
    assert_eq!(
        markers.len(),
        1,
        "completion must fold into the marker the real park path pushed"
    );
    assert!(!markers[0].parked);
}

#[test]
fn completion_does_not_fold_across_bg_completion_chip() {
    // Park → bg task completes (chip lands under the marker) → turn ends:
    // folding would teleport the boundary above the chip, so this flow
    // intentionally keeps both markers.
    let mut agent = running_driver("p1");
    push_parked_tail(&mut agent, "p1", 3);
    agent.scrollback.push_block(RenderBlock::bg_task_completed(
        "sleep 5",
        "task-1",
        std::time::Duration::from_secs(5),
    ));

    push_turn_terminal_marker(
        &mut agent,
        Some(SessionEvent::TurnCompleted {
            elapsed: Some(std::time::Duration::from_secs(9)),
        }),
        Some("p1"),
    );

    let markers = completed_markers(&agent.scrollback);
    assert_eq!(
        markers.len(),
        2,
        "a chip between park and completion keeps both markers"
    );
}

#[test]
fn completion_does_not_fold_committed_parked_marker() {
    // Minimal mode already printed the parked row (print-once): an in-place
    // fold would never reach the terminal — a fresh marker must be pushed.
    let mut agent = running_driver("p1");
    push_parked_tail(&mut agent, "p1", 3);
    let parked_idx = agent.scrollback.len() - 1;
    agent.scrollback.mark_committed(parked_idx);
    agent.scrollback.set_commit_scan_cursor(parked_idx + 1);

    push_turn_terminal_marker(
        &mut agent,
        Some(SessionEvent::TurnCompleted {
            elapsed: Some(std::time::Duration::from_secs(5)),
        }),
        Some("p1"),
    );

    let markers = completed_markers(&agent.scrollback);
    assert_eq!(
        markers.len(),
        2,
        "committed tail must not fold: the completion appends a fresh marker"
    );
    assert!(markers[0].parked, "the committed parked row is untouched");
    assert!(!markers[1].parked, "the fresh marker is the real turn end");
    let fresh = agent.scrollback.last().expect("fresh marker entry");
    assert!(
        !agent.scrollback.is_committed(fresh.id),
        "fresh marker is uncommitted so the commit pass will print it"
    );
}

#[test]
fn completion_does_not_fold_foreign_or_buried_parked_markers() {
    // Different prompt id at the tail: not this turn's park — push normally.
    let mut agent = running_driver("p2");
    push_parked_tail(&mut agent, "p1", 3);
    push_turn_terminal_marker(
        &mut agent,
        Some(SessionEvent::TurnCompleted {
            elapsed: Some(std::time::Duration::from_secs(5)),
        }),
        Some("p2"),
    );
    assert_eq!(
        completed_markers(&agent.scrollback).len(),
        2,
        "a foreign parked marker must not swallow another turn's completion"
    );

    // Buried park (agent output rendered after it): the park no longer
    // explains the tail — the completion pushes its own marker.
    let mut agent = running_driver("p1");
    push_parked_tail(&mut agent, "p1", 3);
    agent.scrollback.push_block(RenderBlock::System(
        crate::scrollback::blocks::SystemMessageBlock::new("resumed"),
    ));
    push_turn_terminal_marker(
        &mut agent,
        Some(SessionEvent::TurnCompleted {
            elapsed: Some(std::time::Duration::from_secs(5)),
        }),
        Some("p1"),
    );
    assert_eq!(
        completed_markers(&agent.scrollback).len(),
        2,
        "a buried parked marker must not fold"
    );
}

#[test]
fn failure_never_folds_into_a_parked_marker() {
    // A parked "Worked for" is a completion-shaped boundary; a failure is a
    // different outcome and must render as its own row beneath it.
    let mut agent = running_driver("p1");
    push_parked_tail(&mut agent, "p1", 3);
    push_turn_terminal_marker(
        &mut agent,
        Some(SessionEvent::TurnFailed {
            error: "boom".into(),
            elapsed: Some(std::time::Duration::from_secs(5)),
        }),
        Some("p1"),
    );
    assert_eq!(
        completed_markers(&agent.scrollback).len(),
        1,
        "the parked marker stays"
    );
    assert!(
        matches!(
            last_session_event(&agent.scrollback),
            Some(SessionEvent::TurnFailed { .. })
        ),
        "the failure renders as its own row"
    );
}
