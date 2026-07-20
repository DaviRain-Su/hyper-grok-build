//! End-to-end coverage for the goal-classifier integration in
//! `drain_goal_updates`. Each test drives an `update_goal(...)`
//! through a real `SessionActor` against a stub subagent
//! coordinator. The stub parses the runner-resolved details
//! path out of the rendered prompt, writes the file, and replies
//! with a canned terminal response — sufficient to exercise the
//! Achieved / NotAchieved / FailOpen / FailClosed / cap-reached
//! / status-changed-mid-fire branches without ever invoking the
//! real sampler. The runner and the gate semantics are covered
//! separately; this module ties both together.
//!
//! Tests that mutate `GROK_GOAL_CLASSIFIER` carry the
//! `serial_test::serial` attribute so env-var state doesn't race
//! between threads when `cargo test` runs the module in parallel —
//! mirrors the precedent set by `resolve_goal_classifier_*`
//! tests in `agent/config.rs`.
use super::support::*;
use super::*;
use crate::session::PromptOrigin;
use serial_test::serial;
use std::collections::VecDeque;
use std::sync::Arc as StdArc;
use std::sync::atomic::{AtomicUsize, Ordering as SeqOrd};
use tokio::sync::Notify;
use xai_grok_tools::implementations::grok_build::task::types::{
    SubagentCancelOutcome, SubagentEvent, SubagentResult,
};
use xai_grok_tools::implementations::grok_build::update_goal::{RejectReason, UpdateGoalInput};
const ENV_FLAG: &str = "GROK_GOAL_CLASSIFIER";
/// Canned subagent response for a single verifier-skeptic spawn.
/// Constructors mirror the verification-stage contract: each
/// `achieved`-flavoured `Response` writes a `refuted: false` JSON
/// verdict + returns `Not Refuted`; `not_achieved` writes
/// `refuted: true` + returns `Refuted`. Method names are preserved
/// from the legacy single-classifier era to keep the test cluster
/// readable across the refactor — the lone-skeptic config
/// (`goal_verifier_skeptic_count = 1` in `create_test_actor`)
/// makes the aggregate verdict match the constructor name.
struct Response {
    text: String,
    verdict_json: Option<String>,
    write_details: bool,
    hold: Option<StdArc<Notify>>,
    subagent_cancelled: bool,
}
impl Response {
    /// Aggregator-level outcome name. At the test-default
    /// `goal_verifier_skeptic_count = 1`, a single `Not Refuted`
    /// vote → aggregate Achieved. The wire-form inversion
    /// (`achieved()` ⇒ `Not Refuted` token + `refuted: false`
    /// JSON) is deliberate: the method names match what the
    /// surrounding drain-path test expects to happen at the
    /// goal-tracker level, not what a single skeptic emits.
    fn achieved() -> Self {
        Self {
            text: "Not Refuted".into(),
            verdict_json: Some(
                "{\"refuted\":false,\"evidence\":\"diff hunk src/foo.rs:1\",\"confidence\":\"high\",\"details_md\":\"# mock skeptic\\n\\nnot refuted\"}"
                    .into(),
            ),
            write_details: true,
            hold: None,
            subagent_cancelled: false,
        }
    }
    fn not_achieved() -> Self {
        Self {
            text: "Refuted".into(),
            verdict_json: Some(
                "{\"refuted\":true,\"evidence\":\"missing test coverage\",\"confidence\":\"high\",\"details_md\":\"# mock skeptic\\n\\nrefuted\"}"
                    .into(),
            ),
            write_details: true,
            hold: None,
            subagent_cancelled: false,
        }
    }
    /// `not_achieved` with caller-chosen evidence so consecutive
    /// rejections carry DISTINCT gap fingerprints — required by the
    /// cap/queue tests to reach the cap without the stall early-exit
    /// firing first.
    fn not_achieved_with(evidence: &str) -> Self {
        Self {
            text: "Refuted".into(),
            verdict_json: Some(format!(
                "{{\"refuted\":true,\"evidence\":\"{evidence}\",\"confidence\":\"high\",\"details_md\":\"# mock skeptic\\n\\nrefuted\"}}"
            )),
            write_details: true,
            hold: None,
            subagent_cancelled: false,
        }
    }
    /// A MEDIUM-confidence refute. Non-decisive (only a high-confidence
    /// skeptic-0 refute short-circuits / binds the outcome), so the
    /// panel fans out and approval rests on the cold quorum.
    fn refuted_medium() -> Self {
        Self {
            text: "Refuted".into(),
            verdict_json: Some(
                "{\"refuted\":true,\"evidence\":\"src/foo.rs:1 weak coverage\",\"confidence\":\"medium\",\"details_md\":\"# mock skeptic\\n\\nrefuted (medium)\"}"
                    .into(),
            ),
            write_details: true,
            hold: None,
            subagent_cancelled: false,
        }
    }
    /// Refute classified as a non-model-fixable blocker
    /// (`contradiction` / `unverifiable`) so the stage routes to the
    /// `Blocked` outcome.
    fn blocked(class: &str) -> Self {
        Self {
            text: "Refuted".into(),
            verdict_json: Some(format!(
                "{{\"refuted\":true,\"evidence\":\"objective conflict\",\"confidence\":\"high\",\"blocking\":\"{class}\",\"details_md\":\"# mock skeptic\\n\\nblocked\"}}"
            )),
            write_details: true,
            hold: None,
            subagent_cancelled: false,
        }
    }
    fn malformed() -> Self {
        Self::malformed_with("acheived")
    }
    /// Malformed terminal token with caller-chosen text so repeated
    /// malformed rejections carry DISTINCT fallback-note fingerprints
    /// (avoids the stall early-exit in the repeated-malformed test).
    fn malformed_with(token: &str) -> Self {
        Self {
            text: token.into(),
            verdict_json: None,
            write_details: true,
            hold: None,
            subagent_cancelled: false,
        }
    }
    fn cancelled_subagent() -> Self {
        Self {
            text: String::new(),
            verdict_json: None,
            write_details: false,
            hold: None,
            subagent_cancelled: true,
        }
    }
    fn with_hold(mut self, n: StdArc<Notify>) -> Self {
        self.hold = Some(n);
        self
    }
}
/// Three NotAchieved rejections with DISTINCT evidence so their gap
/// fingerprints differ — keeps the cap/queue tests on the cap path
/// (no stall early-exit) across attempts.
fn three_distinct_not_achieved() -> VecDeque<Response> {
    VecDeque::from([
        Response::not_achieved_with("src/a.rs:1 missing coverage"),
        Response::not_achieved_with("src/b.rs:2 missing coverage"),
        Response::not_achieved_with("src/c.rs:3 missing coverage"),
    ])
}
/// Subagent coordinator stub. Drains `SubagentEvent`s from the
/// queue plumbed into `ToolContext::subagent_event_tx` and
/// responds to `Spawn` with successive canned [`Response`]s.
/// Per-spawn `(child id, resume_from)` log, shared between the
/// coordinator task and the test.
type SpawnLog = StdArc<parking_lot::Mutex<Vec<(String, Option<String>)>>>;
/// Per-spawn `runtime_overrides.model` in spawn order, so tests can assert
/// which per-index model reached the actual `SubagentRequest`.
type SpawnModelLog = StdArc<parking_lot::Mutex<Vec<Option<String>>>>;
/// Per-spawn rendered skeptic prompt in spawn order, so tests can assert
/// what evidence (e.g. the `FINAL_RESPONSE` breadth anchor) reached the panel.
type SpawnPromptLog = StdArc<parking_lot::Mutex<Vec<String>>>;
/// Per-describe `(subagent_type, harness_agent_type)` in call order, so a test
/// can assert the configured `agent_type` is threaded as the harness, not the
/// subagent type.
type DescribeCallLog = StdArc<parking_lot::Mutex<Vec<(String, Option<String>)>>>;
struct MockCoordinator {
    tx: tokio::sync::mpsc::UnboundedSender<SubagentEvent>,
    spawn_count: StdArc<AtomicUsize>,
    /// Per-spawn `(child id, resume_from)` in spawn order (skeptic 0
    /// first, then the cold fan-out), so tests can assert the resume
    /// round-trip across attempts.
    spawns: SpawnLog,
    /// Per-spawn `runtime_overrides.model`, in spawn order.
    spawn_models: SpawnModelLog,
    /// Per-spawn rendered prompt, in spawn order.
    spawn_prompts: SpawnPromptLog,
    /// Outcome returned for every `DescribeType` round-trip. Defaults to a
    /// fully-capable `Ok` summary (read/search/execute + edit/write) so a
    /// configured role pair commits; tests override it to exercise the
    /// describe-driven fail-open branches.
    describe_outcome: StdArc<
        parking_lot::Mutex<
            xai_grok_tools::implementations::grok_build::task::types::SubagentDescribeOutcome,
        >,
    >,
    /// Per-describe `(subagent_type, harness_agent_type)` in call order.
    describe_calls: DescribeCallLog,
}
/// A fully-capable describe summary (read + search + execute + edit + write)
/// so any role's capability gate passes.
fn capable_describe_outcome()
-> xai_grok_tools::implementations::grok_build::task::types::SubagentDescribeOutcome {
    use xai_grok_tools::implementations::grok_build::task::types::{
        SubagentDescribeOutcome, SubagentTypeSummary,
    };
    use xai_grok_tools::types::tool::ToolKind;
    let mut summary = SubagentTypeSummary {
        can_read: true,
        can_search: true,
        can_execute: true,
        ..Default::default()
    };
    summary
        .tool_names
        .insert(ToolKind::Read, "read_file".into());
    summary.tool_names.insert(ToolKind::Search, "grep".into());
    summary
        .tool_names
        .insert(ToolKind::Execute, "run_terminal_command".into());
    summary
        .tool_names
        .insert(ToolKind::Edit, "search_replace".into());
    SubagentDescribeOutcome::Ok(summary)
}
impl MockCoordinator {
    fn spawn(responses: VecDeque<Response>) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
        let queue = StdArc::new(parking_lot::Mutex::new(responses));
        let spawn_count = StdArc::new(AtomicUsize::new(0));
        let spawns: SpawnLog = StdArc::new(parking_lot::Mutex::new(Vec::new()));
        let spawn_models: SpawnModelLog = StdArc::new(parking_lot::Mutex::new(Vec::new()));
        let spawn_prompts: SpawnPromptLog = StdArc::new(parking_lot::Mutex::new(Vec::new()));
        let describe_outcome = StdArc::new(parking_lot::Mutex::new(capable_describe_outcome()));
        let describe_calls: DescribeCallLog = StdArc::new(parking_lot::Mutex::new(Vec::new()));
        let queue_task = StdArc::clone(&queue);
        let count_task = StdArc::clone(&spawn_count);
        let spawns_task = StdArc::clone(&spawns);
        let spawn_models_task = StdArc::clone(&spawn_models);
        let spawn_prompts_task = StdArc::clone(&spawn_prompts);
        let describe_task = StdArc::clone(&describe_outcome);
        let describe_calls_task = StdArc::clone(&describe_calls);
        tokio::task::spawn_local(async move {
            while let Some(ev) = rx.recv().await {
                match ev {
                    SubagentEvent::DescribeType(req) => {
                        describe_calls_task
                            .lock()
                            .push((req.subagent_type.clone(), req.harness_agent_type.clone()));
                        let outcome = describe_task.lock().clone();
                        let _ = req.respond_to.send(outcome);
                    }
                    SubagentEvent::Spawn(req) => {
                        count_task.fetch_add(1, SeqOrd::SeqCst);
                        spawns_task
                            .lock()
                            .push((req.id.clone(), req.resume_from.clone()));
                        spawn_models_task
                            .lock()
                            .push(req.runtime_overrides.model.clone());
                        spawn_prompts_task.lock().push(req.prompt.clone());
                        let popped = queue_task.lock().pop_front();
                        tokio::task::spawn_local(async move {
                            let Some(r) = popped else {
                                let _ = req.result_tx.send(SubagentResult {
                                    success: false,
                                    error: Some("no canned response".into()),
                                    ..Default::default()
                                });
                                return;
                            };
                            let details = parse_details_path(&req.prompt);
                            let verdict =
                                crate::session::goal_classifier::parse_verdict_path_from_prompt(
                                    &req.prompt,
                                );
                            if let Some(notify) = r.hold.as_ref() {
                                notify.notified().await;
                            }
                            if r.write_details
                                && let Some(p) = details.as_deref()
                            {
                                let _ = tokio::fs::write(p, b"# mock skeptic details\n").await;
                            }
                            if let (Some(p), Some(json)) =
                                (verdict.as_deref(), r.verdict_json.as_deref())
                            {
                                let _ = tokio::fs::write(p, json).await;
                            }
                            let result = if r.subagent_cancelled {
                                SubagentResult {
                                    success: false,
                                    cancelled: true,
                                    error: Some("user aborted".into()),
                                    subagent_id: req.id.clone(),
                                    child_session_id: req.id.clone(),
                                    ..Default::default()
                                }
                            } else {
                                SubagentResult {
                                    success: true,
                                    output: StdArc::from(r.text.as_str()),
                                    subagent_id: req.id.clone(),
                                    child_session_id: req.id.clone(),
                                    ..Default::default()
                                }
                            };
                            let _ = req.result_tx.send(result);
                        });
                    }
                    SubagentEvent::Cancel(c) => {
                        let _ = c.respond_to.send(SubagentCancelOutcome::Cancelled);
                    }
                    _ => {}
                }
            }
        });
        Self {
            tx,
            spawn_count,
            spawns,
            spawn_models,
            spawn_prompts,
            describe_outcome,
            describe_calls,
        }
    }
}
/// Pull the per-skeptic details path out of the rendered verifier
/// prompt. The stub pre-writes the file so the orchestrator's optional
/// details fold-in finds something.
fn parse_details_path(prompt: &str) -> Option<String> {
    crate::session::goal_classifier::parse_skeptic_details_path_from_prompt(prompt)
}
/// Build a Send-free `Arc<SessionActor>` with a fresh active goal,
/// classifier enable flag honoured, events redirected into a
/// tempdir, and (optionally) a subagent-coordinator sender plumbed
/// into `ToolContext`. The tempdir is returned so the caller can
/// scan `events.jsonl` after the test runs.
async fn make_actor(
    coordinator_tx: Option<tokio::sync::mpsc::UnboundedSender<SubagentEvent>>,
    classifier_enabled: bool,
) -> (StdArc<SessionActor>, tempfile::TempDir) {
    make_actor_with_cap(
        coordinator_tx,
        classifier_enabled,
        crate::session::goal_classifier::GOAL_CLASSIFIER_MAX_RUNS_DEFAULT,
    )
    .await
}
/// `make_actor` variant that pins the classifier run cap — used by
/// the cap/queue-mechanics tests so they keep their 3-attempt
/// structure independent of the (now higher) default.
async fn make_actor_with_cap(
    coordinator_tx: Option<tokio::sync::mpsc::UnboundedSender<SubagentEvent>>,
    classifier_enabled: bool,
    max_runs: u32,
) -> (StdArc<SessionActor>, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (gateway_tx, _gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
    let (persistence_tx, _persistence_rx) =
        tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    actor.events = crate::session::events::EventTracker::new(tmp.path());
    actor.goal_enabled = true;
    set_goal_harness_for_tests(&actor);
    actor.goal_classifier_enabled = classifier_enabled;
    actor.goal_classifier_max_runs = max_runs;
    if let Some(tx) = coordinator_tx {
        actor.tool_context.subagent_event_tx = Some(tx);
    }
    actor.goal_tracker.lock().create_goal(
        "test-goal".to_string(),
        "test objective".to_string(),
        None,
        0,
        "2026-01-01T00:00:00Z".to_string(),
        None,
    );
    (StdArc::new(actor), tmp)
}
fn make_completed() -> UpdateGoalInput {
    UpdateGoalInput {
        completed: Some(true),
        message: None,
        blocked_reason: None,
    }
}
fn make_blocked(reason: &str) -> UpdateGoalInput {
    UpdateGoalInput {
        completed: None,
        message: None,
        blocked_reason: Some(reason.to_string()),
    }
}
/// Replace `goal_update_rx` with a fresh channel carrying the
/// supplied commands in FIFO order. Caller may push multiple cmds.
/// The harness discards each `UpdateGoalAck` ack — call
/// [`seed_channel_with_acks`] if the test needs to observe the
/// tool reply.
fn seed_channel(actor: &SessionActor, cmds: Vec<UpdateGoalInput>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    *actor.goal_update_rx.borrow_mut() = Some(rx);
    for cmd in cmds {
        tx.send(xai_grok_tools::implementations::grok_build::update_goal::envelope_for_test(cmd))
            .unwrap();
    }
    drop(tx);
}
/// Like [`seed_channel`] but returns the ack-receivers in the same
/// FIFO order so the test can assert on the verdict-aware tool
/// reply for each input. Receivers are returned in send order.
fn seed_channel_with_acks(
    actor: &SessionActor,
    cmds: Vec<UpdateGoalInput>,
) -> Vec<
    tokio::sync::oneshot::Receiver<
        xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalAck,
    >,
> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    *actor.goal_update_rx.borrow_mut() = Some(rx);
    let mut rxs = Vec::with_capacity(cmds.len());
    for cmd in cmds {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        tx.send((cmd, ack_tx)).unwrap();
        rxs.push(ack_rx);
    }
    drop(tx);
    rxs
}
fn events_log(tmp: &tempfile::TempDir) -> String {
    std::fs::read_to_string(tmp.path().join("events.jsonl")).unwrap_or_default()
}
fn lines_with_type<'a>(log: &'a str, ty: &str) -> Vec<serde_json::Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some(ty))
        .collect()
}
/// Wait (yield) until the actor's `goal_classifier_in_flight`
/// matches `expected`. Bounded by an upper iteration count so a
/// genuine deadlock surfaces as a test timeout, not an infinite
/// loop.
async fn wait_until_in_flight(actor: &SessionActor, expected: bool) {
    for _ in 0..10_000 {
        if actor.goal_classifier_in_flight.load(SeqOrd::SeqCst) == expected {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("timed out waiting for in_flight == {expected}");
}
fn pending_nudge_count(items: &VecDeque<InputItem>) -> usize {
    items
        .iter()
        .filter(|i| matches!(i.origin, PromptOrigin::GoalClassifierNudge))
        .count()
}
fn pending_summary_count(items: &VecDeque<InputItem>) -> usize {
    items
        .iter()
        .filter(|i| matches!(i.origin, PromptOrigin::GoalSummary))
        .count()
}
fn expected_details_path(actor: &SessionActor, attempt: u32) -> String {
    let vid = actor
        .goal_tracker
        .lock()
        .snapshot()
        .expect("goal exists")
        .verifier_id
        .clone();
    crate::session::goal_classifier::format_details_path(&vid, attempt)
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_classifier_disabled_completes_as_today() {
    unsafe { std::env::set_var(ENV_FLAG, "0") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, tmp) = make_actor(None, false).await;
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Complete),
            );
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.classifier_runs_attempted, 0,
                "disabled gate must not reserve an attempt slot",
            );
            assert!(snap.last_classifier_verdict.is_none());
            let state = actor.state.lock().await;
            assert_eq!(
                pending_nudge_count(&state.pending_inputs),
                0,
                "disabled gate must not push a classifier nudge",
            );
            drop(state);
            drop(actor);
            let log = events_log(&tmp);
            assert_eq!(
                lines_with_type(&log, "goal_classifier_fired").len(),
                0,
                "no classifier fire event when disabled:\n{log}",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_classifier_achieved_completes_and_emits_details_path() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(VecDeque::from([Response::achieved()]));
            let (actor, _tmp) = make_actor(Some(coord.tx.clone()), true).await;
            let scratch_details = expected_details_path(&actor, 1);
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::Complete
            );
            assert_eq!(
                snap.last_classifier_verdict,
                Some(crate::session::goal_tracker::GoalClassifierVerdict::Achieved),
            );
            let rescued = actor
                .goal_tracker
                .lock()
                .plan_path()
                .parent()
                .unwrap()
                .join(std::path::Path::new(&scratch_details).file_name().unwrap());
            assert_eq!(
                snap.last_classifier_details_path.as_deref(),
                Some(rescued.to_string_lossy().as_ref()),
            );
            assert!(
                std::fs::read_to_string(&rescued).is_ok(),
                "details file must survive the terminal scratch-root cleanup: {rescued:?}",
            );
            assert_eq!(snap.classifier_runs_attempted, 1);
            assert_eq!(coord.spawn_count.load(SeqOrd::SeqCst), 1);
            let state = actor.state.lock().await;
            assert_eq!(pending_nudge_count(&state.pending_inputs), 0);
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn verifier_panel_records_own_harness_trace_turn_with_footer() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(VecDeque::from([Response::achieved()]));
            let (actor, _tmp) = make_actor(Some(coord.tx.clone()), true).await;
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let spawns = coord.spawn_count.load(SeqOrd::SeqCst);
            assert_eq!(spawns, 1, "test actor runs a single skeptic");
            let turns = actor.chat_state_handle.take_harness_trace_turns().await;
            assert_eq!(turns.len(), 1, "the verification round is one trace turn");
            let items = &turns[0];
            assert_eq!(
                items.len(),
                spawns * 2,
                "each skeptic contributes a task call + result pair",
            );
            let has_footer = items.iter().any(|it| {
                let t = it.text_content();
                t.contains("<subagent_result>") && t.contains("subagent_id:")
            });
            assert!(
                has_footer,
                "skeptic result keeps the <subagent_result> footer",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn verifier_panel_records_one_trace_turn_per_round_and_drain_clears() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(VecDeque::from([
                Response::not_achieved_with("src/a.rs:1 missing coverage"),
                Response::not_achieved_with("src/b.rs:2 missing coverage"),
            ]));
            let (actor, _tmp) = make_actor(Some(coord.tx.clone()), true).await;
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            assert_eq!(
                coord.spawn_count.load(SeqOrd::SeqCst),
                2,
                "two rounds → two skeptic panels",
            );
            let turns = actor.chat_state_handle.take_harness_trace_turns().await;
            assert_eq!(
                turns.len(),
                2,
                "each verification round seals its own trace turn",
            );
            for items in &turns {
                let has_footer = items.iter().any(|it| {
                    let t = it.text_content();
                    t.contains("<subagent_result>") && t.contains("subagent_id:")
                });
                assert!(has_footer, "every round keeps the <subagent_result> footer");
            }
            let after = actor.chat_state_handle.take_harness_trace_turns().await;
            assert!(
                after.is_empty(),
                "drain clears all sealed turns, including the last round",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn non_goal_drain_produces_no_harness_trace_turn() {
    unsafe { std::env::set_var(ENV_FLAG, "0") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_actor(None, false).await;
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let turns = actor.chat_state_handle.take_harness_trace_turns().await;
            assert!(turns.is_empty(), "no verification → no harness trace turn");
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn harness_trace_drain_clears_buffer_even_with_uploads_disabled() {
    use xai_grok_sampling_types::ToolCall;
    use xai_grok_sampling_types::conversation::ConversationItem;
    unsafe { std::env::set_var(ENV_FLAG, "0") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_actor(None, false).await;
            actor.chat_state_handle.append_harness_trace_items(vec![
                ConversationItem::assistant_tool_calls(vec![ToolCall {
                    id: "skeptic-1".into(),
                    name: "task".into(),
                    arguments: "{}".into(),
                }]),
                ConversationItem::tool_result("skeptic-1", "<subagent_result>\nsubagent_id: x"),
            ]);
            actor.chat_state_handle.flush_harness_trace_turn();
            let drained = actor.chat_state_handle.take_harness_trace_turns().await;
            assert_eq!(drained.len(), 1, "the sealed harness turn drains");
            let after = actor.chat_state_handle.take_harness_trace_turns().await;
            assert!(after.is_empty(), "drain clears the buffer with uploads off");
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_classifier_subagent_cancelled_synthesises_refute_vote() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(VecDeque::from([Response::cancelled_subagent()]));
            let (actor, tmp) = make_actor(Some(coord.tx.clone()), true).await;
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::Active,
                "cancellation must NOT auto-complete; it produces a refute vote",
            );
            assert_eq!(
                snap.last_classifier_verdict,
                Some(crate::session::goal_tracker::GoalClassifierVerdict::NotAchieved),
            );
            assert_eq!(snap.classifier_runs_attempted, 1);
            assert_eq!(coord.spawn_count.load(SeqOrd::SeqCst), 1);
            drop(actor);
            let log = events_log(&tmp);
            let skeptics = lines_with_type(&log, "goal_verifier_skeptic_verdict");
            assert!(
                skeptics.iter().any(|v| {
                    v.get("refuted").and_then(|s| s.as_bool()) == Some(true)
                        && v.get("confidence").and_then(|s| s.as_str()) == Some("unknown")
                }),
                "expected synthetic refute vote with confidence=unknown in:\n{log}",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_classifier_not_achieved_persists_gaps_without_nudge() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(VecDeque::from([Response::not_achieved()]));
            let (actor, _tmp) = make_actor(Some(coord.tx.clone()), true).await;
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::Active,
                "attempt 1 of 3 must keep the goal Active",
            );
            assert_eq!(snap.classifier_runs_attempted, 1);
            assert_eq!(
                snap.last_classifier_verdict,
                Some(crate::session::goal_tracker::GoalClassifierVerdict::NotAchieved),
            );
            assert!(
                snap.last_classifier_gaps.is_some(),
                "NotAchieved must persist gaps for the continuation directive",
            );
            let state = actor.state.lock().await;
            assert_eq!(
                pending_nudge_count(&state.pending_inputs),
                0,
                "the legacy nudge turn must no longer be queued",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_classifier_cap_reached_pauses_with_backoff() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(three_distinct_not_achieved());
            let (actor, _tmp) = make_actor_with_cap(Some(coord.tx.clone()), true, 3).await;
            let expected_3 = expected_details_path(&actor, 3);
            for _ in 0..3 {
                seed_channel(&actor, vec![make_completed()]);
                actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            }
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::BackOffPaused,
            );
            assert_eq!(snap.classifier_runs_attempted, 3);
            let msg = snap.pause_message.as_deref().unwrap_or_default();
            assert!(
                msg.contains(&expected_3),
                "pause_message must mention the details path: {msg}",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_classifier_stall_early_exit_pauses_with_no_progress() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(
                VecDeque::from([Response::not_achieved(), Response::not_achieved()]),
            );
            let (actor, tmp) = make_actor(Some(coord.tx.clone()), true).await;
            use xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalAck;
            let mut last_ack = None;
            for _ in 0..2 {
                let mut rxs = seed_channel_with_acks(&actor, vec![make_completed()]);
                let rx = rxs.pop().unwrap();
                actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
                last_ack = Some(rx.await.expect("ack delivered"));
            }
            assert!(
                matches!(last_ack, Some(UpdateGoalAck::ClassifierStalled { attempt : 2,
                .. })),
                "second identical rejection must ack ClassifierStalled at attempt 2; got {last_ack:?}",
            );
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.status, crate ::session::goal_tracker::GoalStatus::NoProgressPaused,
                "a stall (not the cap) must pause as NoProgressPaused, distinct from back-off",
            );
            assert_eq!(
                snap.classifier_runs_attempted, 2,
                "stall must fire at attempt 2, before exhausting the cap",
            );
            let log = events_log(&tmp);
            let paused = lines_with_type(&log, "goal_auto_paused");
            assert!(
                paused.iter().any(| v | v.get("reason").and_then(| x | x.as_str()) ==
                Some("no_progress")),
                "stall pause must emit goal_auto_paused reason=no_progress; log={log}",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_classifier_blocked_outcome_pauses_for_user_and_consolidates_queue() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            use xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalAck;
            let coord = MockCoordinator::spawn(VecDeque::from([Response::blocked("unverifiable")]));
            let (actor, tmp) = make_actor(Some(coord.tx.clone()), true).await;
            let rxs = seed_channel_with_acks(&actor, vec![make_completed(), make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let mut acks = Vec::new();
            for rx in rxs {
                acks.push(rx.await.expect("ack delivered"));
            }
            assert!(
                matches!(acks[0], UpdateGoalAck::ClassifierBlocked { .. }),
                "first completion must ack ClassifierBlocked; got {:?}",
                acks[0],
            );
            assert!(
                matches!(
                    acks[1],
                    UpdateGoalAck::Rejected {
                        reason: RejectReason::DroppedAfterPauseInDrain,
                        ..
                    }
                ),
                "second completion after a blocked pause must be consolidated, not \
                     per-entry FailOpen; got {:?}",
                acks[1],
            );
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::Blocked,
                "all-blocking rejection must pause as Blocked (needs-user)",
            );
            assert_eq!(
                snap.classifier_runs_attempted, 0,
                "the blocked attempt slot must be rolled back",
            );
            let log = events_log(&tmp);
            let paused = lines_with_type(&log, "goal_auto_paused");
            assert!(
                paused
                    .iter()
                    .any(|v| v.get("reason").and_then(|x| x.as_str()) == Some("verification")),
                "blocked pause must emit goal_auto_paused reason=verification; log={log}",
            );
            let cleared = lines_with_type(&log, "goal_classifier_pending_queue_cleared");
            assert_eq!(
                cleared.len(),
                1,
                "blocked mid-drain must consolidate the leftover into ONE cleared event; log={log}",
            );
            assert!(
                lines_with_type(&log, "goal_classifier_fail_open").is_empty(),
                "the dropped second completion must NOT emit a per-entry fail_open; log={log}",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_classifier_cap_takes_precedence_over_stall() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            use xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalAck;
            let coord = MockCoordinator::spawn(VecDeque::from([
                Response::not_achieved(),
                Response::not_achieved(),
            ]));
            let (actor, _tmp) = make_actor_with_cap(Some(coord.tx.clone()), true, 2).await;
            let mut acks = Vec::new();
            for _ in 0..2 {
                let mut rxs = seed_channel_with_acks(&actor, vec![make_completed()]);
                let rx = rxs.pop().unwrap();
                actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
                acks.push(rx.await.expect("ack delivered"));
            }
            assert!(
                matches!(
                    acks[0],
                    UpdateGoalAck::ClassifierNotAchieved { attempt: 1, .. }
                ),
                "first rejection acks NotAchieved; got {:?}",
                acks[0],
            );
            assert!(
                matches!(
                    acks[1],
                    UpdateGoalAck::ClassifierCapReached { attempt: 2, .. }
                ),
                "at-cap identical-fingerprint rejection must ack CapReached, NOT Stalled; got {:?}",
                acks[1],
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
async fn goal_classifier_synthetic_in_flight_path_never_stalls() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_actor(None, true).await;
            actor.goal_classifier_in_flight.store(true, SeqOrd::SeqCst);
            for _ in 0..2 {
                seed_channel(&actor, vec![make_completed()]);
                actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            }
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::Active,
                "the synthetic in-flight path must never trip the stall early-exit",
            );
            assert_eq!(snap.classifier_runs_attempted, 2);
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_classifier_stall_pause_consolidates_mid_drain_queue() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            use xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalAck;
            let coord = MockCoordinator::spawn(VecDeque::from([
                Response::not_achieved(),
                Response::not_achieved(),
            ]));
            let (actor, tmp) = make_actor(Some(coord.tx.clone()), true).await;
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let rxs = seed_channel_with_acks(&actor, vec![make_completed(), make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let mut acks = Vec::new();
            for rx in rxs {
                acks.push(rx.await.expect("ack delivered"));
            }
            assert!(
                matches!(acks[0], UpdateGoalAck::ClassifierStalled { attempt: 2, .. }),
                "first drain-2 completion must stall; got {:?}",
                acks[0],
            );
            assert!(
                matches!(
                    acks[1],
                    UpdateGoalAck::Rejected {
                        reason: RejectReason::DroppedAfterPauseInDrain,
                        ..
                    }
                ),
                "second completion after a stall pause must be consolidated; got {:?}",
                acks[1],
            );
            let log = events_log(&tmp);
            assert_eq!(
                lines_with_type(&log, "goal_classifier_pending_queue_cleared").len(),
                1,
                "stall mid-drain must consolidate into ONE cleared event; log={log}",
            );
            assert!(
                lines_with_type(&log, "goal_classifier_fail_open").is_empty(),
                "the dropped completion must NOT emit a per-entry fail_open; log={log}",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_classifier_post_blocked_resume_does_not_immediately_restall() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            use xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalAck;
            let coord = MockCoordinator::spawn(VecDeque::from([
                Response::not_achieved_with("src/a.rs:1 missing coverage"),
                Response::blocked("contradiction"),
                Response::not_achieved_with("src/a.rs:1 missing coverage"),
            ]));
            let (actor, _tmp) = make_actor(Some(coord.tx.clone()), true).await;
            let mut rxs = seed_channel_with_acks(&actor, vec![make_completed()]);
            let rx = rxs.pop().unwrap();
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            assert!(matches!(
                rx.await.unwrap(),
                UpdateGoalAck::ClassifierNotAchieved { attempt: 1, .. }
            ));
            let mut rxs = seed_channel_with_acks(&actor, vec![make_completed()]);
            let rx = rxs.pop().unwrap();
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            assert!(matches!(
                rx.await.unwrap(),
                UpdateGoalAck::ClassifierBlocked { .. }
            ));
            {
                let o = actor.goal_tracker.lock().snapshot().cloned().unwrap();
                assert_eq!(o.status, crate::session::goal_tracker::GoalStatus::Blocked);
                assert_eq!(o.classifier_stall_count, 0, "Blocked must reset the streak");
                assert!(o.last_gap_fingerprint.is_none());
            }
            assert!(actor.goal_tracker.lock().resume());
            let mut rxs = seed_channel_with_acks(&actor, vec![make_completed()]);
            let rx = rxs.pop().unwrap();
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            assert!(
                matches!(
                    rx.await.unwrap(),
                    UpdateGoalAck::ClassifierNotAchieved { .. }
                ),
                "a post-Blocked repeat of the old fingerprint must nudge, not re-stall",
            );
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Active),
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_classifier_double_completion_in_one_drain_does_not_double_fire() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let hold = StdArc::new(Notify::new());
            let coord = MockCoordinator::spawn(VecDeque::from([
                Response::achieved().with_hold(StdArc::clone(&hold))
            ]));
            let (actor, _tmp) = make_actor(Some(coord.tx.clone()), true).await;
            seed_channel(&actor, vec![make_completed()]);
            let drain_actor = StdArc::clone(&actor);
            let drain1 = tokio::task::spawn_local(async move {
                drain_actor
                    .drain_goal_updates(0, DrainPurpose::TurnEnd)
                    .await;
            });
            wait_until_in_flight(&actor, true).await;
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let snap_mid = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(snap_mid.classifier_runs_attempted, 2);
            let synth_path = expected_details_path(&actor, 2);
            assert!(
                tokio::fs::metadata(&synth_path).await.is_ok(),
                "synthetic details file must exist at {synth_path}",
            );
            {
                let state = actor.state.lock().await;
                assert_eq!(pending_nudge_count(&state.pending_inputs), 0);
            }
            hold.notify_one();
            drain1.await.unwrap();
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::Complete
            );
            assert_eq!(
                snap.last_classifier_verdict,
                Some(crate::session::goal_tracker::GoalClassifierVerdict::Achieved),
            );
            assert_eq!(
                coord.spawn_count.load(SeqOrd::SeqCst),
                1,
                "only the first completion spawns; the 2nd short-circuits at Guard 3",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_classifier_mid_turn_drain_defers_completion_to_turn_end() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(VecDeque::from([Response::achieved()]));
            let (actor, _tmp) = make_actor(Some(coord.tx.clone()), true).await;
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::MidTurn).await;
            let snap_mid = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap_mid.status,
                crate::session::goal_tracker::GoalStatus::Active,
            );
            assert_eq!(snap_mid.classifier_runs_attempted, 0);
            assert_eq!(actor.pending_classifier_completions.lock().len(), 1);
            assert_eq!(
                coord.spawn_count.load(SeqOrd::SeqCst),
                0,
                "mid-turn drain must NOT fire classifier",
            );
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::Complete
            );
            assert_eq!(snap.classifier_runs_attempted, 1);
            assert_eq!(coord.spawn_count.load(SeqOrd::SeqCst), 1);
            assert_eq!(actor.pending_classifier_completions.lock().len(), 0);
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_classifier_status_changed_during_await_drops_result() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let hold = StdArc::new(Notify::new());
            let coord = MockCoordinator::spawn(VecDeque::from([
                Response::achieved().with_hold(StdArc::clone(&hold))
            ]));
            let (actor, tmp) = make_actor(Some(coord.tx.clone()), true).await;
            seed_channel(&actor, vec![make_completed()]);
            let drain_actor = StdArc::clone(&actor);
            let drain = tokio::task::spawn_local(async move {
                drain_actor
                    .drain_goal_updates(0, DrainPurpose::TurnEnd)
                    .await;
            });
            wait_until_in_flight(&actor, true).await;
            tokio::time::sleep(Duration::from_millis(5)).await;
            assert!(
                actor
                    .goal_tracker
                    .lock()
                    .pause(crate::session::goal_tracker::GoalPauseReason::User),
            );
            hold.notify_one();
            drain.await.unwrap();
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::UserPaused,
                "user pause wins over classifier verdict",
            );
            assert_eq!(
                snap.classifier_runs_attempted, 0,
                "discarded fire must roll back its reserved attempt slot",
            );
            drop(actor);
            let log = events_log(&tmp);
            let fail_opens = lines_with_type(&log, "goal_classifier_fail_open");
            let event = fail_opens
                .iter()
                .find(|v| {
                    v.get("reason").and_then(|s| s.as_str()) == Some("goal_not_active_at_resolve")
                })
                .unwrap_or_else(|| panic!("expected fail_open event in:\n{log}"));
            let latency = event
                .get("latency_ms")
                .and_then(|n| n.as_u64())
                .unwrap_or(0);
            assert!(
                latency > 0,
                "discarded-fire latency must be non-zero, got {latency}",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
/// A turn cancel dropping the drain future mid-verification must clear the
/// `verifying_in_flight` latch — otherwise "Verifying…" sticks forever.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn verifying_latch_clears_when_drain_future_dropped_mid_verification() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let hold = StdArc::new(Notify::new());
            let coord = MockCoordinator::spawn(VecDeque::from([
                Response::achieved().with_hold(StdArc::clone(&hold))
            ]));
            let (actor, _tmp) = make_actor(Some(coord.tx.clone()), true).await;
            seed_channel(&actor, vec![make_completed()]);
            let drain_actor = StdArc::clone(&actor);
            let drain = tokio::task::spawn_local(async move {
                drain_actor
                    .drain_goal_updates(0, DrainPurpose::TurnEnd)
                    .await;
            });
            wait_until_in_flight(&actor, true).await;
            assert!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .unwrap()
                    .verifying_in_flight,
                "latch must be set while the panel runs",
            );
            drain.abort();
            let _ = drain.await;
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert!(
                !snap.verifying_in_flight,
                "dropped drain future must clear the Verifying latch",
            );
            assert!(
                !actor.goal_classifier_in_flight.load(SeqOrd::SeqCst),
                "InFlightGuard must also have released the re-entry flag",
            );
            assert_eq!(
                snap.classifier_runs_attempted, 0,
                "cancel mid-verification must refund the reserved attempt slot",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
/// Guard 3's synthetic NotAchieved ran no panel; it must preserve a real
/// prior verdict's actionable gaps instead of clobbering them.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn synthetic_concurrent_verdict_preserves_real_gaps() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            const REAL_GAPS: &str = "- [skeptic 0, high] login flow still 500s";
            let hold = StdArc::new(Notify::new());
            let coord = MockCoordinator::spawn(VecDeque::from([
                Response::achieved().with_hold(StdArc::clone(&hold))
            ]));
            let (actor, _tmp) = make_actor(Some(coord.tx.clone()), true).await;
            actor
                .goal_tracker
                .lock()
                .snapshot_mut()
                .unwrap()
                .last_classifier_gaps = Some(REAL_GAPS.to_string());
            seed_channel(&actor, vec![make_completed()]);
            let drain_actor = StdArc::clone(&actor);
            let drain1 = tokio::task::spawn_local(async move {
                drain_actor
                    .drain_goal_updates(0, DrainPurpose::TurnEnd)
                    .await;
            });
            wait_until_in_flight(&actor, true).await;
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let snap_mid = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(snap_mid.classifier_runs_attempted, 2, "synthetic counted");
            assert_eq!(
                snap_mid.last_classifier_gaps.as_deref(),
                Some(REAL_GAPS),
                "synthetic verdict must preserve the real gaps, not clobber them",
            );
            assert_eq!(
                snap_mid.last_classifier_verdict,
                Some(crate::session::goal_tracker::GoalClassifierVerdict::NotAchieved),
            );
            assert_eq!(
                snap_mid.last_classifier_details_path.as_deref(),
                Some(expected_details_path(&actor, 2).as_str()),
            );
            hold.notify_one();
            drain1.await.unwrap();
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
/// A mid-turn-deferred `completed: true` must not survive a pause (auto or
/// `/goal pause`) — replaying it after resume burns a cap attempt.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn pause_clears_deferred_classifier_completions() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_actor(None, true).await;
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::MidTurn).await;
            assert_eq!(actor.pending_classifier_completions.lock().len(), 1);
            actor
                .auto_pause_goal_if_active(crate::session::goal_tracker::GoalPauseReason::User)
                .await;
            assert_eq!(
                actor.pending_classifier_completions.lock().len(),
                0,
                "auto-pause must drop deferred completions",
            );
            let (actor, _tmp) = make_actor(None, true).await;
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::MidTurn).await;
            assert_eq!(actor.pending_classifier_completions.lock().len(), 1);
            let _ = actor
                .execute_builtin_slash_command(BuiltinAction::GoalPause)
                .await;
            assert_eq!(
                actor.pending_classifier_completions.lock().len(),
                0,
                "/goal pause must drop deferred completions",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
/// A synthetic cap pause landing mid-panel must NOT have the owner's slot
/// refunded: post-cap accounting includes that attempt.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn synthetic_cap_pause_mid_panel_keeps_reserved_slot() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let hold = StdArc::new(Notify::new());
            let coord = MockCoordinator::spawn(VecDeque::from([
                Response::achieved().with_hold(StdArc::clone(&hold))
            ]));
            let (actor, _tmp) = make_actor_with_cap(Some(coord.tx.clone()), true, 2).await;
            seed_channel(&actor, vec![make_completed()]);
            let drain_actor = StdArc::clone(&actor);
            let drain1 = tokio::task::spawn_local(async move {
                drain_actor
                    .drain_goal_updates(0, DrainPurpose::TurnEnd)
                    .await;
            });
            wait_until_in_flight(&actor, true).await;
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::BackOffPaused),
                "synthetic attempt 2/2 must cap-pause",
            );
            hold.notify_one();
            drain1.await.unwrap();
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::BackOffPaused
            );
            assert_eq!(
                snap.classifier_runs_attempted, 2,
                "cap pause accounting must not be refunded below the cap",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
/// Pins the `NoProgressPaused` skip-list arm: a stall pause is
/// classifier-driven, so a discarded fire keeps its slot.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn stall_pause_mid_panel_keeps_reserved_slot() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let hold = StdArc::new(Notify::new());
            let coord = MockCoordinator::spawn(VecDeque::from([
                Response::achieved().with_hold(StdArc::clone(&hold))
            ]));
            let (actor, _tmp) = make_actor(Some(coord.tx.clone()), true).await;
            seed_channel(&actor, vec![make_completed()]);
            let drain_actor = StdArc::clone(&actor);
            let drain = tokio::task::spawn_local(async move {
                drain_actor
                    .drain_goal_updates(0, DrainPurpose::TurnEnd)
                    .await;
            });
            wait_until_in_flight(&actor, true).await;
            assert!(
                actor
                    .goal_tracker
                    .lock()
                    .pause(crate::session::goal_tracker::GoalPauseReason::NoProgress),
            );
            hold.notify_one();
            drain.await.unwrap();
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::NoProgressPaused
            );
            assert_eq!(
                snap.classifier_runs_attempted, 1,
                "a stall pause is classifier-driven; the discarded fire must keep its slot",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
/// A completion deferred while the verification stage awaits must be
/// dropped by the FailOpenAchieved completion.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn fail_open_achieved_drops_concurrently_deferred_completions() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_actor(None, true).await;
            seed_channel(&actor, vec![make_completed()]);
            let drain_actor = StdArc::clone(&actor);
            let drain = tokio::task::spawn_local(async move {
                drain_actor
                    .drain_goal_updates(0, DrainPurpose::TurnEnd)
                    .await;
            });
            tokio::task::yield_now().await;
            actor
                .pending_classifier_completions
                .lock()
                .push_back(make_completed());
            drain.await.unwrap();
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::Complete,
                "no-coordinator stage must fail open to Achieved",
            );
            assert_eq!(
                actor.pending_classifier_completions.lock().len(),
                0,
                "FailOpenAchieved must drop completions deferred while the stage ran",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
/// A completion deferred by a concurrent mid-turn drain while the panel is
/// held must be dropped by the budget-limit transition.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn budget_limit_drops_concurrently_deferred_completions() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let hold = StdArc::new(Notify::new());
            let coord = MockCoordinator::spawn(VecDeque::from([
                Response::not_achieved().with_hold(StdArc::clone(&hold))
            ]));
            let (actor, _tmp) = make_actor(Some(coord.tx.clone()), true).await;
            actor
                .goal_tracker
                .lock()
                .snapshot_mut()
                .unwrap()
                .token_budget = Some(0);
            seed_channel(&actor, vec![make_completed()]);
            let queue_actor = StdArc::clone(&actor);
            let task = tokio::task::spawn_local(async move {
                queue_actor.maybe_queue_goal_continuation().await;
            });
            wait_until_in_flight(&actor, true).await;
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::MidTurn).await;
            assert_eq!(actor.pending_classifier_completions.lock().len(), 1);
            hold.notify_one();
            task.await.unwrap();
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::BudgetLimited),
                "NotAchieved keeps the goal Active; the zero budget then limits it",
            );
            assert_eq!(
                actor.pending_classifier_completions.lock().len(),
                0,
                "budget limit must drop completions deferred while the panel ran",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
/// `setup_goal` (a new `/goal <objective>`) and `/goal clear` are the two
/// non-pause exits that must also drop a previous goal's deferred claims.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_create_and_clear_drop_deferred_classifier_completions() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_actor(None, true).await;
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::MidTurn).await;
            assert_eq!(actor.pending_classifier_completions.lock().len(), 1);
            let _ = actor.setup_goal("a brand new objective", None).await;
            assert_eq!(
                actor.pending_classifier_completions.lock().len(),
                0,
                "new-goal setup must drop the previous goal's deferred claims",
            );
            let (actor, _tmp) = make_actor(None, true).await;
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::MidTurn).await;
            assert_eq!(actor.pending_classifier_completions.lock().len(), 1);
            let _ = actor
                .execute_builtin_slash_command(BuiltinAction::GoalClear)
                .await;
            assert_eq!(
                actor.pending_classifier_completions.lock().len(),
                0,
                "/goal clear must drop deferred completions",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
/// A completion deferred while the panel ran must not survive the Achieved
/// verdict into the next goal's first turn-end drain.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn achieved_verdict_drops_concurrently_deferred_completions() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let hold = StdArc::new(Notify::new());
            let coord = MockCoordinator::spawn(VecDeque::from([
                Response::achieved().with_hold(StdArc::clone(&hold))
            ]));
            let (actor, _tmp) = make_actor(Some(coord.tx.clone()), true).await;
            seed_channel(&actor, vec![make_completed()]);
            let drain_actor = StdArc::clone(&actor);
            let drain1 = tokio::task::spawn_local(async move {
                drain_actor
                    .drain_goal_updates(0, DrainPurpose::TurnEnd)
                    .await;
            });
            wait_until_in_flight(&actor, true).await;
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::MidTurn).await;
            assert_eq!(actor.pending_classifier_completions.lock().len(), 1);
            hold.notify_one();
            drain1.await.unwrap();
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Complete),
            );
            assert_eq!(
                actor.pending_classifier_completions.lock().len(),
                0,
                "Achieved must drop completions deferred while the panel ran",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_classifier_malformed_terminal_response_synthesises_refute_vote() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(VecDeque::from([Response::malformed()]));
            let (actor, tmp) = make_actor(Some(coord.tx.clone()), true).await;
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.last_classifier_verdict,
                Some(crate::session::goal_tracker::GoalClassifierVerdict::NotAchieved),
            );
            assert_eq!(snap.classifier_runs_attempted, 1);
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::Active
            );
            {
                let state = actor.state.lock().await;
                assert_eq!(pending_nudge_count(&state.pending_inputs), 0);
            }
            drop(actor);
            let log = events_log(&tmp);
            let skeptics = lines_with_type(&log, "goal_verifier_skeptic_verdict");
            assert!(
                skeptics.iter().any(|v| {
                    v.get("refuted").and_then(|s| s.as_bool()) == Some(true)
                        && v.get("confidence").and_then(|s| s.as_str()) == Some("unknown")
                }),
                "expected synthetic refute vote with confidence=unknown in:\n{log}",
            );
            let agg = lines_with_type(&log, "goal_verifier_aggregate_verdict");
            assert!(
                agg.iter().any(|v| {
                    v.get("refuted_count").and_then(|s| s.as_u64()) == Some(1)
                        && v.get("total").and_then(|s| s.as_u64()) == Some(1)
                        && v.get("achieved").and_then(|s| s.as_bool()) == Some(false)
                }),
                "expected aggregate 1/1 not-achieved in:\n{log}",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_classifier_repeated_malformed_eventually_pauses_with_backoff() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(VecDeque::from([
                Response::malformed_with("nonsense one"),
                Response::malformed_with("nonsense two"),
                Response::malformed_with("nonsense three"),
            ]));
            let (actor, _tmp) = make_actor_with_cap(Some(coord.tx.clone()), true, 3).await;
            for _ in 0..3 {
                seed_channel(&actor, vec![make_completed()]);
                actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            }
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::BackOffPaused,
            );
            assert_eq!(snap.classifier_runs_attempted, 3);
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_classifier_nudge_suppresses_subsequent_goal_summary() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_actor(None, true).await;
            {
                let (respond_to, _) = tokio::sync::oneshot::channel();
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(InputItem {
                    prompt_id: "goal-classifier-nudge-seed".to_string(),
                    prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                        "seed".to_string(),
                    ))],
                    prompt_mode: crate::session::plan_mode::PromptMode::Agent,
                    trace_gcs_config: None,
                    artifact_tracker: None,
                    client_identifier: None,
                    screen_mode: None,
                    verbatim: true,
                    json_schema: None,
                    origin: PromptOrigin::GoalClassifierNudge,
                    task_wake_fallback: None,
                    respond_to,
                    persist_ack: None,
                    parsed_prompt_tx: None,
                    queue_meta: None,
                    send_now: false,
                });
            }
            actor.maybe_queue_goal_continuation().await;
            let state = actor.state.lock().await;
            assert_eq!(
                pending_summary_count(&state.pending_inputs),
                0,
                "pending GoalClassifierNudge must suppress a GoalSummary push",
            );
            assert_eq!(pending_nudge_count(&state.pending_inputs), 1);
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_classifier_block_seen_short_circuits_pending_completion() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(VecDeque::from([]));
            let (actor, _tmp) = make_actor(Some(coord.tx.clone()), true).await;
            actor.goal_blocked_streak.store(2, SeqOrd::Relaxed);
            seed_channel(&actor, vec![make_blocked("3rd failure"), make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::Blocked,
                "blocked transition wins over the follow-up completed",
            );
            assert_eq!(
                snap.classifier_runs_attempted, 0,
                "block_seen short-circuit must prevent any classifier fire",
            );
            assert_eq!(
                coord.spawn_count.load(SeqOrd::SeqCst),
                0,
                "no subagent spawn when block_seen short-circuits",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_classifier_active_guard_drops_non_active_completion() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(VecDeque::from([]));
            let (actor, tmp) = make_actor(Some(coord.tx.clone()), true).await;
            assert!(
                actor
                    .goal_tracker
                    .lock()
                    .pause(crate::session::goal_tracker::GoalPauseReason::User),
            );
            actor.goal_blocked_streak.store(2, SeqOrd::Relaxed);
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::UserPaused,
            );
            assert_eq!(snap.classifier_runs_attempted, 0);
            assert_eq!(coord.spawn_count.load(SeqOrd::SeqCst), 0);
            assert_eq!(
                actor.goal_blocked_streak.load(SeqOrd::Relaxed),
                0,
                "completed must reset blocked_streak regardless of which guard fires",
            );
            drop(actor);
            let log = events_log(&tmp);
            assert!(
                lines_with_type(&log, "goal_classifier_fail_open")
                    .iter()
                    .any(|v| v.get("reason").and_then(|s| s.as_str())
                        == Some("goal_not_active_at_resolve")),
                "expected GoalNotActiveAtResolve fail_open event in:\n{log}",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_classifier_in_flight_short_circuit_resets_blocked_streak() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(VecDeque::from([]));
            let (actor, _tmp) = make_actor(Some(coord.tx.clone()), true).await;
            actor.goal_classifier_in_flight.store(true, SeqOrd::SeqCst);
            seed_channel(&actor, vec![make_blocked("first")]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            seed_channel(&actor, vec![make_blocked("second")]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            assert_eq!(actor.goal_blocked_streak.load(SeqOrd::Relaxed), 2);
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            assert_eq!(
                actor.goal_blocked_streak.load(SeqOrd::Relaxed),
                0,
                "in-flight short-circuit must NOT preserve blocked_streak",
            );
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::Active,
                "synthetic NotAchieved at attempt 1 keeps goal Active",
            );
            assert_eq!(snap.classifier_runs_attempted, 1);
            assert_eq!(
                coord.spawn_count.load(SeqOrd::SeqCst),
                0,
                "in-flight short-circuit must NOT spawn the subagent",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_classifier_in_flight_second_completion_accounts_attempt_and_writes_synthetic_file() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(VecDeque::from([]));
            let (actor, _tmp) = make_actor(Some(coord.tx.clone()), true).await;
            actor.goal_classifier_in_flight.store(true, SeqOrd::SeqCst);
            seed_channel(&actor, vec![make_completed(), make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.classifier_runs_attempted, 2,
                "two in-flight completions must reserve two attempt slots",
            );
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::Active
            );
            assert_eq!(
                snap.last_classifier_verdict,
                Some(crate::session::goal_tracker::GoalClassifierVerdict::NotAchieved),
            );
            let path_1 = expected_details_path(&actor, 1);
            let path_2 = expected_details_path(&actor, 2);
            assert!(
                tokio::fs::metadata(&path_1).await.is_ok(),
                "synthetic details file 1 must exist at {path_1}",
            );
            assert!(
                tokio::fs::metadata(&path_2).await.is_ok(),
                "synthetic details file 2 must exist at {path_2}",
            );
            {
                let state = actor.state.lock().await;
                assert_eq!(
                    pending_nudge_count(&state.pending_inputs),
                    0,
                    "no nudge turn is queued; feedback rides the continuation directive",
                );
            }
            assert_eq!(
                coord.spawn_count.load(SeqOrd::SeqCst),
                0,
                "in-flight short-circuit must never spawn the subagent",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_classifier_in_flight_second_completion_at_cap_pauses_with_backoff() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(VecDeque::from([]));
            let (actor, _tmp) = make_actor_with_cap(Some(coord.tx.clone()), true, 3).await;
            {
                let mut tracker = actor.goal_tracker.lock();
                let o = tracker.snapshot_mut().unwrap();
                o.classifier_runs_attempted = 2;
                o.classifier_max_runs = Some(3);
            }
            actor.goal_classifier_in_flight.store(true, SeqOrd::SeqCst);
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::BackOffPaused,
                "synthetic NotAchieved at cap must pause with BackOff",
            );
            assert_eq!(snap.classifier_runs_attempted, 3);
            let expected = expected_details_path(&actor, 3);
            assert!(
                snap.pause_message
                    .as_deref()
                    .is_some_and(|m| m.contains(&expected)),
                "pause_message must mention details path {expected}",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
/// SECURITY: when `ensure_goal_scratch_root` fails because a local attacker
/// pre-planted a file at the predictable scratch root, the synthetic
/// NotAchieved path must NOT record, return, or point users / tools at the
/// scratch-rooted details path it never wrote. The goal still accounts the
/// attempt and pauses at cap, but with no "See …" pointer to attacker-
/// controllable content.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_classifier_in_flight_synthetic_details_omitted_when_scratch_root_squatted() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(VecDeque::from([]));
            let (actor, _tmp) = make_actor_with_cap(Some(coord.tx.clone()), true, 3).await;
            {
                let mut tracker = actor.goal_tracker.lock();
                let o = tracker.snapshot_mut().unwrap();
                o.classifier_runs_attempted = 2;
                o.classifier_max_runs = Some(3);
            }
            let vid = actor
                .goal_tracker
                .lock()
                .snapshot()
                .unwrap()
                .verifier_id
                .clone();
            let squatted_root = crate::session::goal_tracker::goal_scratch_root(&vid);
            let _ = std::fs::remove_dir_all(&squatted_root);
            std::fs::write(&squatted_root, b"attacker-planted").unwrap();
            assert!(
                crate::session::goal_tracker::ensure_goal_scratch_root(&vid).is_err(),
                "test premise: the file-squatted root must make ensure fail",
            );
            let squatted_details = expected_details_path(&actor, 3);
            actor.goal_classifier_in_flight.store(true, SeqOrd::SeqCst);
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(snap.classifier_runs_attempted, 3);
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::BackOffPaused,
            );
            assert_eq!(
                snap.last_classifier_details_path, None,
                "must not record the scratch-rooted path the harness never wrote",
            );
            assert!(
                !snap
                    .pause_message
                    .as_deref()
                    .unwrap_or("")
                    .contains(&squatted_details),
                "pause message must not point at the unwritten path",
            );
            assert!(
                tokio::fs::metadata(&squatted_details).await.is_err(),
                "no synthetic details file must exist under a squatted root",
            );
            let _ = std::fs::remove_file(&squatted_root);
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_classifier_env_override_disables_when_remote_enabled() {
    unsafe { std::env::set_var(ENV_FLAG, "0") };
    let remote_value: Option<bool> = Some(true);
    let resolved = crate::agent::config::BoolFlag::env(ENV_FLAG)
        .feature_flag(remote_value)
        .default(false)
        .resolve();
    assert_eq!(
        resolved.source,
        crate::agent::config::ConfigSource::Env,
        "env presence must take precedence over remote",
    );
    assert!(
        !resolved.value,
        "GROK_GOAL_CLASSIFIER=0 must disable even with remote=true",
    );
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_actor(None, resolved.value).await;
            assert!(!actor.goal_classifier_enabled);
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::Complete,
                "env-disabled gate must complete immediately (today's behaviour)",
            );
            assert_eq!(snap.classifier_runs_attempted, 0);
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
/// The drain wires the cached `goal_verifier_skeptic_count` field
/// into `VerificationStageInputs`. Every `make_actor`-built test
/// uses N=1 for spawn-count parity with the legacy single-classifier
/// asserts; this test explicitly flips an actor to N=2 and runs a
/// medium-refute skeptic 0 + a clearing cold skeptic 1. Under
/// variant-C, approval rests on the COLD panel (skeptic 1), so a
/// non-decisive skeptic-0 refute does not block — the goal completes.
/// Regression guard: a refactor that silently drops the cached field
/// (or fans out 1 spawn instead of 2) is caught here.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_verification_stage_n2_skeptic0_medium_refute_cold_clears_completes() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(VecDeque::from([
                Response::refuted_medium(),
                Response::achieved(),
            ]));
            let (actor, _tmp) = make_actor(Some(coord.tx.clone()), true).await;
            let actor = StdArc::new(SessionActor {
                goal_verifier_skeptic_count: 2,
                goal_role_models: Default::default(),
                goal_use_current_model_only: false,
                ..StdArc::try_unwrap(actor).ok().expect("single-owner actor")
            });
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::Complete,
                "cold skeptic 1 clears → cold quorum approves → goal complete",
            );
            assert_eq!(
                coord.spawn_count.load(SeqOrd::SeqCst),
                2,
                "exactly two skeptics must spawn at N=2",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
/// Variant-C at the e2e drain level: skeptic 0 NOT-refuted but the
/// cold skeptic 1 refutes → skeptic 0's not-refuted vote does NOT
/// carry the quorum → NotAchieved, so the goal stays Active (does not
/// complete) at attempt 1 of the default cap.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_verification_stage_n2_skeptic0_clear_cold_refute_does_not_complete() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(VecDeque::from([
                Response::achieved(),
                Response::not_achieved(),
            ]));
            let (actor, _tmp) = make_actor(Some(coord.tx.clone()), true).await;
            let actor = StdArc::new(SessionActor {
                goal_verifier_skeptic_count: 2,
                goal_role_models: Default::default(),
                goal_use_current_model_only: false,
                ..StdArc::try_unwrap(actor).ok().expect("single-owner actor")
            });
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::Active,
                "skeptic-0 not-refuted cannot carry the cold quorum → NotAchieved → stays Active",
            );
            assert_eq!(
                snap.last_classifier_verdict,
                Some(crate::session::goal_tracker::GoalClassifierVerdict::NotAchieved),
            );
            assert_eq!(coord.spawn_count.load(SeqOrd::SeqCst), 2);
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
/// Part-A persistence round-trip through the drain: a NotAchieved
/// N=2 attempt writes skeptic 0's child id back onto the orchestration,
/// and the NEXT attempt threads it back in as `resume_from` for the
/// resumed skeptic 0 (cold skeptic 1 stays fresh). Drives two attempts
/// in one drain (distinct gaps so the stall early-exit never fires).
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn goal_verification_stage_n2_drain_persists_and_resumes_skeptic0() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(VecDeque::from([
                Response::achieved(),
                Response::not_achieved_with("attempt-1 gap"),
                Response::achieved(),
                Response::not_achieved_with("attempt-2 gap"),
            ]));
            let (actor, _tmp) = make_actor(Some(coord.tx.clone()), true).await;
            let actor = StdArc::new(SessionActor {
                goal_verifier_skeptic_count: 2,
                goal_role_models: Default::default(),
                goal_use_current_model_only: false,
                ..StdArc::try_unwrap(actor).ok().expect("single-owner actor")
            });
            seed_channel(&actor, vec![make_completed(), make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(snap.classifier_runs_attempted, 2, "two attempts ran");
            assert!(
                snap.skeptic0_session_id.is_some(),
                "drain must write skeptic 0's child id back onto the orchestration",
            );
            let spawns = coord.spawns.lock().clone();
            assert_eq!(spawns.len(), 4, "2 skeptics × 2 attempts");
            let attempt1_skeptic0_id = spawns[0].0.clone();
            assert_eq!(spawns[0].1, None, "attempt 1 skeptic 0 is a cold spawn");
            assert_eq!(spawns[1].1, None, "cold skeptic 1 never resumes");
            assert_eq!(
                spawns[2].1.as_deref(),
                Some(attempt1_skeptic0_id.as_str()),
                "attempt 2 skeptic 0 resumes attempt 1's persisted child id",
            );
            assert_eq!(spawns[3].1, None, "cold skeptic 1 never resumes");
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
async fn update_goal_tool_blocks_until_classifier_verdict_when_enabled() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let queue: VecDeque<Response> = vec![Response::achieved()].into();
            let coordinator = MockCoordinator::spawn(queue);
            let (actor, _tmp) = make_actor(Some(coordinator.tx.clone()), true).await;
            let mut rxs = seed_channel_with_acks(&actor, vec![make_completed()]);
            let ack_rx = rxs.pop().expect("one ack");
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let ack = ack_rx.await.expect("ack delivered");
            assert!(
                matches!(ack,
                xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalAck::ClassifierAchieved
                { .. },),
                "classifier-enabled drain must deliver Achieved ack; got {ack:?}",
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn update_goal_tool_returns_immediately_when_classifier_disabled() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_actor(None, false).await;
            let mut rxs = seed_channel_with_acks(&actor, vec![make_completed()]);
            let ack_rx = rxs.pop().expect("one ack");
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let ack = ack_rx.await.expect("ack delivered");
            assert!(
                matches!(ack,
                xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalAck::CompletedWithoutClassifier,)
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn update_goal_tool_returns_error_when_classifier_in_flight_for_previous_call() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_actor(None, true).await;
            actor.goal_classifier_in_flight.store(true, SeqOrd::SeqCst);
            let mut rxs = seed_channel_with_acks(&actor, vec![make_completed()]);
            let ack_rx = rxs.pop().expect("one ack");
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let ack = ack_rx.await.expect("ack delivered");
            use xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalAck;
            match ack {
                UpdateGoalAck::ClassifierConcurrentInFlight {
                    attempt, max_runs, ..
                } => {
                    assert_eq!(attempt, 1);
                    assert_eq!(
                        max_runs,
                        crate::session::goal_classifier::GOAL_CLASSIFIER_MAX_RUNS_DEFAULT,
                    );
                }
                other => {
                    panic!(
                        "expected ClassifierConcurrentInFlight {{ attempt: 1, .. }}; got {other:?}"
                    )
                }
            }
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn update_goal_tool_does_not_deadlock_on_mid_turn_completion() {
    use xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalAck;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_actor(None, true).await;
            let mut rxs = seed_channel_with_acks(&actor, vec![make_completed()]);
            let ack_rx = rxs.pop().expect("one ack");
            actor.drain_goal_updates(0, DrainPurpose::MidTurn).await;
            let ack = tokio::time::timeout(std::time::Duration::from_secs(1), ack_rx)
                .await
                .expect("ack must resolve within MidTurn defer window")
                .expect("ack receiver delivered");
            let pending_depth = match ack {
                UpdateGoalAck::DeferredToTurnEnd { pending_depth } => pending_depth,
                other => panic!("expected DeferredToTurnEnd; got {other:?}"),
            };
            assert_eq!(pending_depth, 1);
            assert_eq!(actor.pending_classifier_completions.lock().len(), 1);
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn update_goal_tool_deferred_input_fires_classifier_at_turn_end() {
    use xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalAck;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let queue: VecDeque<Response> = vec![Response::achieved()].into();
            let coordinator = MockCoordinator::spawn(queue);
            let (actor, _tmp) = make_actor(Some(coordinator.tx.clone()), true).await;
            let mut rxs = seed_channel_with_acks(&actor, vec![make_completed()]);
            let ack_rx = rxs.pop().expect("one ack");
            actor.drain_goal_updates(0, DrainPurpose::MidTurn).await;
            let ack = tokio::time::timeout(std::time::Duration::from_secs(1), ack_rx)
                .await
                .expect("ack must resolve within MidTurn defer window")
                .expect("ack receiver delivered");
            assert!(matches!(ack, UpdateGoalAck::DeferredToTurnEnd { .. }));
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            assert_eq!(
                coordinator.spawn_count.load(SeqOrd::SeqCst),
                1,
                "classifier must fire at turn-end for the parked completion",
            );
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Complete),
                "Achieved verdict at turn-end completes the goal",
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn update_goal_tool_returns_immediately_for_blocked_reason() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_actor(None, true).await;
            let mut rxs = seed_channel_with_acks(
                &actor,
                vec![make_blocked("transient")],
            );
            let ack_rx = rxs.pop().expect("one ack");
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let ack = ack_rx.await.expect("ack delivered");
            assert!(
                matches!(ack,
                xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalAck::Accepted
                { .. },)
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn goal_classifier_post_cap_completion_does_not_emit_attempt_zero_fail_open() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coordinator = MockCoordinator::spawn(three_distinct_not_achieved());
            let (actor, tmp) = make_actor_with_cap(Some(coordinator.tx.clone()), true, 3).await;
            for _ in 0..3 {
                seed_channel(&actor, vec![make_completed()]);
                actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            }
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::BackOffPaused),
                "3 NotAchieved verdicts must pause via BackOff (cap reached)",
            );
            let log_before = events_log(&tmp);
            let fail_open_before = lines_with_type(&log_before, "goal_classifier_fail_open").len();
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let log = events_log(&tmp);
            let fail_open = lines_with_type(&log, "goal_classifier_fail_open");
            assert_eq!(
                fail_open.len(),
                fail_open_before,
                "post-cap drop MUST NOT add a `fail_open` event; we now emit \
                     `dropped_after_cap` instead",
            );
            let dropped = lines_with_type(&log, "goal_classifier_dropped_after_cap");
            assert!(
                !dropped.is_empty(),
                "post-cap drop must emit `goal_classifier_dropped_after_cap`; log={log}",
            );
            let attempts_seen = dropped
                .last()
                .and_then(|v| v.get("attempts_seen").and_then(|x| x.as_u64()))
                .expect("attempts_seen field present");
            assert_eq!(
                attempts_seen, 3,
                "attempts_seen must equal the cap (3), NOT 0 (the old broken value)",
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn goal_classifier_pending_queue_drained_on_cap_pause() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coordinator = MockCoordinator::spawn(three_distinct_not_achieved());
            let (actor, tmp) = make_actor_with_cap(Some(coordinator.tx.clone()), true, 3)
                .await;
            for _ in 0..2 {
                seed_channel(&actor, vec![make_completed()]);
                actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            }
            seed_channel(&actor, vec![make_completed(), make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::MidTurn).await;
            assert_eq!(
                actor.pending_classifier_completions.lock().len(), 2,
                "mid-turn drain must defer both completions",
            );
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            assert_eq!(
                actor.goal_tracker.lock().status(), Some(crate
                ::session::goal_tracker::GoalStatus::BackOffPaused),
            );
            assert_eq!(
                actor.pending_classifier_completions.lock().len(), 0,
                "pending queue must be empty after cap-reached drain",
            );
            let log = events_log(&tmp);
            let cleared = lines_with_type(&log, "goal_classifier_pending_queue_cleared");
            assert_eq!(
                cleared.len(), 1,
                "exactly one `pending_queue_cleared` summary event must fire on cap-pause; log={log}",
            );
            let dropped = cleared[0]
                .get("dropped")
                .and_then(|x| x.as_u64())
                .expect("dropped field present");
            assert_eq!(dropped, 2);
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn goal_classifier_sequential_drain_four_completions_three_attempts_then_cap() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            use xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalAck;
            let coordinator = MockCoordinator::spawn(three_distinct_not_achieved());
            let (actor, _tmp) = make_actor_with_cap(
                    Some(coordinator.tx.clone()),
                    true,
                    3,
                )
                .await;
            let rxs = seed_channel_with_acks(
                &actor,
                vec![
                    make_completed(), make_completed(), make_completed(),
                    make_completed(),
                ],
            );
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            assert_eq!(
                coordinator.spawn_count.load(SeqOrd::SeqCst), 3,
                "classifier must fire at most `max_runs` (3) times across 4 completions",
            );
            assert_eq!(
                actor.goal_tracker.lock().status(), Some(crate
                ::session::goal_tracker::GoalStatus::BackOffPaused),
            );
            let mut acks = Vec::new();
            for rx in rxs {
                acks.push(rx.await.expect("ack delivered"));
            }
            assert!(
                matches!(& acks[0], UpdateGoalAck::ClassifierNotAchieved { attempt : 1,
                .. }),
                "first completion ack must be ClassifierNotAchieved attempt=1; got {:?}",
                acks[0],
            );
            assert!(
                matches!(& acks[1], UpdateGoalAck::ClassifierNotAchieved { attempt : 2,
                .. }),
                "second completion ack must be ClassifierNotAchieved attempt=2; got {:?}",
                acks[1],
            );
            assert!(
                matches!(& acks[2], UpdateGoalAck::ClassifierCapReached { attempt : 3, ..
                }),
                "third completion ack must be ClassifierCapReached attempt=3; got {:?}",
                acks[2],
            );
            match &acks[3] {
                UpdateGoalAck::Rejected { reason, .. } => {
                    assert_eq!(
                        * reason, RejectReason::DroppedAfterPauseInDrain,
                        "post-cap drop must surface DroppedAfterPauseInDrain",
                    );
                }
                other => {
                    panic!(
                        "fourth completion must be Rejected (cap-mid-drain drop); got {other:?}",
                    )
                }
            }
        })
        .await;
}
/// Concurrent in-flight short-circuit: while the first classifier
/// is parked on `notify`, a second `update_goal(completed: true)`
/// must ack as `ClassifierConcurrentInFlight` (NOT success).
#[tokio::test(flavor = "current_thread")]
async fn goal_classifier_concurrent_in_flight_short_circuits_second_completion() {
    use xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalAck;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let notify = StdArc::new(Notify::new());
            let queue: VecDeque<Response> =
                vec![Response::achieved().with_hold(notify.clone())].into();
            let coordinator = MockCoordinator::spawn(queue);
            let (actor, _tmp) = make_actor(Some(coordinator.tx.clone()), true).await;
            let actor_for_first = actor.clone();
            let first_handle = tokio::task::spawn_local(async move {
                seed_channel(&actor_for_first, vec![make_completed()]);
                actor_for_first
                    .drain_goal_updates(0, DrainPurpose::TurnEnd)
                    .await;
            });
            for _ in 0..2_000 {
                if actor.goal_classifier_in_flight.load(SeqOrd::SeqCst) {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert!(
                actor.goal_classifier_in_flight.load(SeqOrd::SeqCst),
                "first classifier must have entered in-flight",
            );
            let mut rxs = seed_channel_with_acks(&actor, vec![make_completed()]);
            let second_ack_rx = rxs.pop().unwrap();
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let second_ack = second_ack_rx.await.expect("second ack delivered");
            match second_ack {
                UpdateGoalAck::ClassifierConcurrentInFlight { attempt, .. } => {
                    assert_eq!(attempt, 2);
                }
                other => panic!("expected ClassifierConcurrentInFlight; got {other:?}"),
            }
            notify.notify_one();
            first_handle.await.expect("first drain completes");
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn rejected_ack_post_cap_carries_correct_reason() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coordinator = MockCoordinator::spawn(three_distinct_not_achieved());
            let (actor, _tmp) = make_actor_with_cap(Some(coordinator.tx.clone()), true, 3).await;
            for _ in 0..3 {
                seed_channel(&actor, vec![make_completed()]);
                actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            }
            let mut rxs = seed_channel_with_acks(&actor, vec![make_completed()]);
            let ack_rx = rxs.pop().unwrap();
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let ack = ack_rx.await.expect("ack delivered");
            use xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalAck;
            match ack {
                UpdateGoalAck::Rejected { reason, detail } => {
                    assert_eq!(reason, RejectReason::PostCap);
                    assert!(detail.contains("cap already reached"));
                }
                other => panic!("expected Rejected{{PostCap}}; got {other:?}"),
            }
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn pending_queue_overflow_acks_all_as_deferred_and_caps_at_pending_queue_cap() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, tmp) = make_actor(None, true).await;
            let mut inputs = Vec::new();
            for _ in 0..(super::GOAL_CLASSIFIER_PENDING_QUEUE_CAP + 1) {
                inputs.push(make_completed());
            }
            let rxs = seed_channel_with_acks(&actor, inputs);
            actor.drain_goal_updates(0, DrainPurpose::MidTurn).await;
            use xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalAck;
            for rx in rxs {
                let ack = rx.await.expect("ack delivered");
                assert!(
                    matches!(ack, UpdateGoalAck::DeferredToTurnEnd { .. }),
                    "every MidTurn defer must ack immediately as DeferredToTurnEnd; got {ack:?}",
                );
            }
            assert_eq!(
                actor.pending_classifier_completions.lock().len(),
                super::GOAL_CLASSIFIER_PENDING_QUEUE_CAP,
                "FIFO eviction must hold the queue at cap",
            );
            let log = events_log(&tmp);
            let queue_full = lines_with_type(&log, "goal_classifier_fail_closed");
            assert!(
                queue_full.iter().any(| v | v.get("reason").and_then(| x | x.as_str()) ==
                Some("pending_queue_full")),
                "PendingQueueFull telemetry must fire; log={log}",
            );
        })
        .await;
}
#[test]
fn render_ack_classifier_achieved_is_success() {
    use xai_grok_tools::implementations::grok_build::update_goal::{
        UpdateGoalAck, render_ack_into_output,
    };
    let out = render_ack_into_output(UpdateGoalAck::ClassifierAchieved {
        details_path: "/tmp/details.md".to_string(),
    })
    .expect("Achieved must be Ok");
    assert!(out.success);
    assert!(out.summary.contains("Achieved"));
    assert!(out.summary.contains("/tmp/details.md"));
}
#[test]
fn render_ack_classifier_fail_open_achieved_clarifies_no_verdict() {
    use xai_grok_tools::implementations::grok_build::update_goal::{
        UpdateGoalAck, render_ack_into_output,
    };
    let out =
        render_ack_into_output(UpdateGoalAck::ClassifierFailOpenAchieved { reason: "timeout" })
            .expect("FailOpen must be Ok (treated as achieved)");
    assert!(out.success);
    assert!(out.summary.contains("fail-open"));
    assert!(out.summary.contains("timeout"));
    assert!(out.summary.contains("No classifier verdict"));
}
#[test]
fn render_ack_not_achieved_is_tool_error_with_correct_code() {
    use xai_grok_tools::implementations::grok_build::update_goal::{
        UpdateGoalAck, render_ack_into_output,
    };
    let err = render_ack_into_output(UpdateGoalAck::ClassifierNotAchieved {
        details_path: "/tmp/details.md".to_string(),
        attempt: 2,
        max_runs: 3,
    })
    .expect_err("NotAchieved must be Err");
    assert_eq!(tool_error_code(&err), "goal_classifier_not_achieved");
}
#[test]
fn render_ack_cap_reached_is_tool_error_with_cap_code() {
    use xai_grok_tools::implementations::grok_build::update_goal::{
        UpdateGoalAck, render_ack_into_output,
    };
    let err = render_ack_into_output(UpdateGoalAck::ClassifierCapReached {
        details_path: "/tmp/details.md".to_string(),
        attempt: 3,
    })
    .expect_err("CapReached must be Err");
    assert_eq!(tool_error_code(&err), "goal_classifier_cap_reached");
}
/// Extract the per-cause error code from a `ToolError` built via
/// `ToolError::custom(code, ...)`. The code lives in
/// `details["code"]`; `kind` is always `ToolErrorKind::Custom`
/// for custom-built errors.
fn tool_error_code(err: &xai_tool_runtime::ToolError) -> &str {
    err.details
        .as_ref()
        .and_then(|v| v.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("<no code>")
}
#[test]
fn render_ack_rejected_uses_reason_error_code() {
    use xai_grok_tools::implementations::grok_build::update_goal::{
        UpdateGoalAck, render_ack_into_output,
    };
    for (reason, want_code) in all_reject_reasons() {
        let err = render_ack_into_output(UpdateGoalAck::Rejected {
            reason: *reason,
            detail: "x".to_string(),
        })
        .expect_err("Rejected must be Err");
        assert_eq!(
            tool_error_code(&err),
            *want_code,
            "Rejected{{{reason:?}}} must map to {want_code}; got code={:?}",
            tool_error_code(&err),
        );
    }
}
/// Every `RejectReason` variant paired with its expected stable
/// error code. A future addition to the enum MUST add a row here
/// — `reject_reasons_complete_matrix` pins the completeness
/// invariant via an exhaustive match.
fn all_reject_reasons() -> &'static [(RejectReason, &'static str)] {
    &[
        (RejectReason::BlockSeenInDrain, "goal_update_block_seen"),
        (
            RejectReason::BlockedAgainstNonActive,
            "goal_update_blocked_against_non_active",
        ),
        (RejectReason::PostCap, "goal_update_post_cap"),
        (RejectReason::NonActive, "goal_update_non_active"),
        (RejectReason::PendingQueueEvicted, "goal_update_evicted"),
        (
            RejectReason::DroppedAfterPauseInDrain,
            "goal_update_dropped_after_pause",
        ),
        (
            RejectReason::OrchestrationVanished,
            "goal_update_no_orchestration",
        ),
        (
            RejectReason::StatusChangedDuringClassifier,
            "goal_update_status_changed",
        ),
        (
            RejectReason::InFlightOrchestrationVanished,
            "goal_update_in_flight_orchestration_vanished",
        ),
        (
            RejectReason::HarnessDisabled,
            "goal_update_harness_disabled",
        ),
    ]
}
#[test]
fn reject_reasons_complete_matrix() {
    for (variant, _code) in all_reject_reasons() {
        match variant {
            RejectReason::BlockSeenInDrain
            | RejectReason::BlockedAgainstNonActive
            | RejectReason::PostCap
            | RejectReason::NonActive
            | RejectReason::PendingQueueEvicted
            | RejectReason::DroppedAfterPauseInDrain
            | RejectReason::OrchestrationVanished
            | RejectReason::StatusChangedDuringClassifier
            | RejectReason::InFlightOrchestrationVanished
            | RejectReason::HarnessDisabled => {}
        }
    }
    assert_eq!(
        all_reject_reasons().len(),
        10,
        "all_reject_reasons must contain every RejectReason variant",
    );
    let mut codes: Vec<&'static str> = all_reject_reasons().iter().map(|(_, c)| *c).collect();
    codes.sort_unstable();
    let before = codes.len();
    codes.dedup();
    assert_eq!(
        codes.len(),
        before,
        "every RejectReason error code must be unique",
    );
    for code in codes {
        assert!(!code.is_empty(), "error code must be non-empty");
    }
}
use crate::session::acp_session::GoalRoleModelConfig;
use crate::session::acp_session::goal::{PanelResolveCache, RoleCapability};
use xai_grok_tools::implementations::grok_build::task::types::SubagentDescribeOutcome;
fn role_pair(model: &str, agent_type: &str) -> crate::util::config::GoalRoleModel {
    crate::util::config::GoalRoleModel {
        model: model.to_string(),
        agent_type: agent_type.to_string(),
    }
}
/// One-line model catalog builder: `(model_id, user_selectable)` entries.
fn catalog_with(
    entries: &[(&str, bool)],
) -> indexmap::IndexMap<String, crate::agent::config::ModelEntry> {
    let mut m = indexmap::IndexMap::new();
    for (id, selectable) in entries {
        let mut info = crate::agent::config::ModelInfo::fallback(id);
        info.user_selectable = *selectable;
        m.insert(
            (*id).to_string(),
            crate::agent::config::ModelEntry {
                info,
                api_key: None,
                env_key: None,
                auth_provider: None,
                api_base_url: None,
            },
        );
    }
    m
}
fn test_auth_manager_for_models() -> std::sync::Arc<crate::auth::AuthManager> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mgr = std::sync::Arc::new(crate::auth::AuthManager::new(
        tmp.path(),
        crate::auth::GrokComConfig::default(),
    ));
    std::mem::forget(tmp);
    mgr
}
/// `make_actor_with_cap` variant that wires per-role model selection:
/// skeptic count + pool, kill-switch, a frozen `skeptic_model_assignment`,
/// and a populated model catalog (for the entitlement gate).
async fn make_role_model_actor(
    coordinator_tx: Option<tokio::sync::mpsc::UnboundedSender<SubagentEvent>>,
    skeptic_count: u32,
    pool: Vec<crate::util::config::GoalRoleModel>,
    frozen: Vec<crate::util::config::GoalRoleModel>,
    kill_switch: bool,
    catalog: indexmap::IndexMap<String, crate::agent::config::ModelEntry>,
) -> (StdArc<SessionActor>, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (gateway_tx, _gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
    let (persistence_tx, _persistence_rx) =
        tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    actor.events = crate::session::events::EventTracker::new(tmp.path());
    actor.goal_enabled = true;
    set_goal_harness_for_tests(&actor);
    actor.goal_classifier_enabled = true;
    actor.goal_verifier_skeptic_count = skeptic_count;
    actor.goal_use_current_model_only = kill_switch;
    actor.goal_role_models = GoalRoleModelConfig {
        planner: Default::default(),
        strategist: Default::default(),
        skeptic_pool: pool,
    };
    if !catalog.is_empty() {
        actor.models_manager = crate::agent::models::ModelsManager::new(
            None,
            catalog,
            acp::ModelId::new("auto"),
            test_auth_manager_for_models(),
            crate::agent::config::Config::default(),
        );
    }
    if let Some(tx) = coordinator_tx {
        actor.tool_context.subagent_event_tx = Some(tx);
    }
    actor.goal_tracker.lock().create_goal(
        "test-goal".to_string(),
        "test objective".to_string(),
        None,
        0,
        "2026-01-01T00:00:00Z".to_string(),
        None,
    );
    if !frozen.is_empty() {
        actor
            .goal_tracker
            .lock()
            .snapshot_mut()
            .expect("goal exists")
            .skeptic_model_assignment = frozen;
    }
    (StdArc::new(actor), tmp)
}
/// Drive `resolve_goal_role_override` directly with a controlled catalog +
/// describe outcome; return the resulting override + the events log so the
/// test can assert the fail-open reason / resolved event.
async fn run_resolve(
    pair: &crate::util::config::GoalRoleModel,
    capability: RoleCapability,
    catalog: &[(&str, bool)],
    describe: SubagentDescribeOutcome,
) -> (crate::session::goal_planner::RoleSpawnOverride, String) {
    let coord = MockCoordinator::spawn(VecDeque::new());
    *coord.describe_outcome.lock() = describe;
    let (actor, tmp) = make_role_model_actor(
        Some(coord.tx.clone()),
        1,
        Vec::new(),
        Vec::new(),
        false,
        indexmap::IndexMap::new(),
    )
    .await;
    let available = catalog_with(catalog);
    let mut cache = PanelResolveCache::default();
    let ov = actor
        .resolve_goal_role_override(
            "skeptic",
            Some(0),
            pair,
            capability,
            &coord.tx,
            &available,
            &mut cache,
        )
        .await;
    drop(actor);
    let log = events_log(&tmp);
    (ov, log)
}
#[tokio::test(flavor = "current_thread")]
async fn resolve_role_override_model_unknown_fails_open() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (ov, log) = run_resolve(
                &role_pair("absent-model", "general-purpose"),
                RoleCapability::Skeptic,
                &[("other-model", true)],
                capable_describe_outcome(),
            )
            .await;
            assert!(!ov.is_explicit(), "unknown model must inherit");
            let evs = lines_with_type(&log, "goal_role_model_fail_open");
            assert_eq!(evs.len(), 1, "{log}");
            assert_eq!(evs[0]["reason"], "model_unknown");
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn resolve_role_override_model_unauthorized_fails_open() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (ov, log) = run_resolve(
                &role_pair("blocked-model", "general-purpose"),
                RoleCapability::Skeptic,
                &[("blocked-model", false)],
                capable_describe_outcome(),
            )
            .await;
            assert!(!ov.is_explicit(), "unauthorized model must inherit");
            let evs = lines_with_type(&log, "goal_role_model_fail_open");
            assert_eq!(evs.len(), 1, "{log}");
            assert_eq!(evs[0]["reason"], "model_unauthorized");
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn resolve_role_override_toolset_outcomes_fail_open() {
    let cases = [
        (
            SubagentDescribeOutcome::Unknown {
                available: vec!["explore".into()],
            },
            "toolset_unknown",
        ),
        (
            SubagentDescribeOutcome::NotAllowed {
                allowed: vec!["explore".into()],
            },
            "toolset_not_allowed",
        ),
        (SubagentDescribeOutcome::Disabled, "toolset_disabled"),
        (SubagentDescribeOutcome::Unavailable, "toolset_unavailable"),
    ];
    for (describe, expected_reason) in cases {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (ov, log) = run_resolve(
                    &role_pair("good-model", "general-purpose"),
                    RoleCapability::Skeptic,
                    &[("good-model", true)],
                    describe,
                )
                .await;
                assert!(!ov.is_explicit(), "{expected_reason} must inherit");
                let evs = lines_with_type(&log, "goal_role_model_fail_open");
                assert_eq!(evs.len(), 1, "{log}");
                assert_eq!(evs[0]["reason"], expected_reason);
            })
            .await;
    }
}
#[tokio::test(flavor = "current_thread")]
async fn resolve_role_override_toolset_incapable_fails_open() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut summary =
                xai_grok_tools::implementations::grok_build::task::types::SubagentTypeSummary {
                    can_read: true,
                    ..Default::default()
                };
            summary.tool_names.insert(
                xai_grok_tools::types::tool::ToolKind::Read,
                "read_file".into(),
            );
            let (ov, log) = run_resolve(
                &role_pair("good-model", "general-purpose"),
                RoleCapability::Skeptic,
                &[("good-model", true)],
                SubagentDescribeOutcome::Ok(summary),
            )
            .await;
            assert!(!ov.is_explicit(), "incapable toolset must inherit");
            let evs = lines_with_type(&log, "goal_role_model_fail_open");
            assert_eq!(evs.len(), 1, "{log}");
            assert_eq!(evs[0]["reason"], "toolset_incapable");
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn resolve_role_override_all_pass_commits_and_emits_resolved() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (ov, log) = run_resolve(
                &role_pair("good-model", "general-purpose"),
                RoleCapability::Skeptic,
                &[("good-model", true)],
                capable_describe_outcome(),
            )
            .await;
            assert_eq!(ov.model.as_deref(), Some("good-model"));
            assert_eq!(ov.agent_type.as_deref(), Some("general-purpose"));
            assert!(lines_with_type(&log, "goal_role_model_fail_open").is_empty());
            let evs = lines_with_type(&log, "goal_role_model_resolved");
            assert_eq!(evs.len(), 1, "{log}");
            assert_eq!(evs[0]["model_id"], "good-model");
            assert_eq!(evs[0]["agent_type"], "general-purpose");
            assert_eq!(evs[0]["source"], "remote");
        })
        .await;
}
/// A strict-but-unrepresentable harness (`codex`) fails open with the distinct
/// `harness_flavor_unsupported` reason. Honored:
/// `grok-build-plan` (non-strict), and `opencode` (non-strict + unrepresentable
/// — proving the gate keys on `is_strict`, not representability alone).
#[tokio::test(flavor = "current_thread")]
async fn resolve_role_override_harness_flavor_representability_gate() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (ov, log) = run_resolve(
                &role_pair("good-model", "codex"),
                RoleCapability::Skeptic,
                &[("good-model", true)],
                capable_describe_outcome(),
            )
            .await;
            assert!(!ov.is_explicit(), "codex must fail open to inherit: {log}");
            let evs = lines_with_type(&log, "goal_role_model_fail_open");
            assert_eq!(evs.len(), 1, "{log}");
            assert_eq!(evs[0]["reason"], "harness_flavor_unsupported");
            let honored = ["grok-build-plan", "opencode"];
            for harness in honored {
                let (ov, log) = run_resolve(
                    &role_pair("good-model", harness),
                    RoleCapability::Skeptic,
                    &[("good-model", true)],
                    capable_describe_outcome(),
                )
                .await;
                assert_eq!(
                    ov.agent_type.as_deref(),
                    Some(harness),
                    "{harness} must be honored (committed), not fail open",
                );
                assert!(
                    lines_with_type(&log, "goal_role_model_fail_open").is_empty(),
                    "{harness} must not fail open: {log}",
                );
            }
        })
        .await;
}
/// A transient `Unavailable` describe outcome on the first panel index must
/// NOT be memoized: a later index sharing the same `agent_type` re-describes,
/// so a one-off coordinator hiccup does not fail open every skeptic in the
/// panel. Drives two `resolve_goal_role_override` calls through one shared
/// `PanelResolveCache`, flipping the coordinator from `Unavailable` to a
/// capable `Ok` between them.
#[tokio::test(flavor = "current_thread")]
async fn resolve_role_override_unavailable_not_cached_reprobes_next_index() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(VecDeque::new());
            *coord.describe_outcome.lock() = SubagentDescribeOutcome::Unavailable;
            let (actor, tmp) = make_role_model_actor(
                Some(coord.tx.clone()),
                2,
                Vec::new(),
                Vec::new(),
                false,
                indexmap::IndexMap::new(),
            )
            .await;
            let available = catalog_with(&[("good-model", true)]);
            let mut cache = PanelResolveCache::default();
            let pair = role_pair("good-model", "general-purpose");
            let ov0 = actor
                .resolve_goal_role_override(
                    "skeptic",
                    Some(0),
                    &pair,
                    RoleCapability::Skeptic,
                    &coord.tx,
                    &available,
                    &mut cache,
                )
                .await;
            assert!(
                !ov0.is_explicit(),
                "transient Unavailable must fail open for index 0"
            );
            *coord.describe_outcome.lock() = capable_describe_outcome();
            let ov1 = actor
                .resolve_goal_role_override(
                    "skeptic",
                    Some(1),
                    &pair,
                    RoleCapability::Skeptic,
                    &coord.tx,
                    &available,
                    &mut cache,
                )
                .await;
            assert_eq!(
                ov1.model.as_deref(),
                Some("good-model"),
                "same agent_type must re-describe after a transient Unavailable"
            );
            assert_eq!(ov1.agent_type.as_deref(), Some("general-purpose"));
            drop(actor);
            let _ = tmp;
        })
        .await;
}
/// Explicit pair → committed override AND tool_names drawn from the role's
/// describe summary (`name_override`-aware), with the `{TOOLSET_TOOLS}` block —
/// exercising resolve_inherit + resolve_goal_role_override + role_tool_names_from
/// in the single-role build path, reusing the SAME cached describe summary.
#[tokio::test(flavor = "current_thread")]
async fn single_role_override_explicit_builds_tool_names_from_summary() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            use xai_grok_tools::implementations::grok_build::task::types::{
                SubagentDescribeOutcome, SubagentTypeSummary,
            };
            use xai_grok_tools::types::tool::ToolKind;
            let coord = MockCoordinator::spawn(VecDeque::new());
            let mut summary = SubagentTypeSummary {
                can_read: true,
                can_search: true,
                can_execute: true,
                ..Default::default()
            };
            summary
                .tool_names
                .insert(ToolKind::Read, "cursor_read".into());
            summary
                .tool_names
                .insert(ToolKind::Write, "cursor_write".into());
            *coord.describe_outcome.lock() = SubagentDescribeOutcome::Ok(summary);
            let (actor, _tmp) = make_role_model_actor(
                Some(coord.tx.clone()),
                1,
                Vec::new(),
                Vec::new(),
                false,
                catalog_with(&[("good-model", true)]),
            )
            .await;
            let choice = crate::agent::config::GoalRoleModelChoice::Explicit(role_pair(
                "good-model",
                "general-purpose",
            ));
            let (ov, tn, inherit_tn) = actor
                .resolve_goal_single_role_override(
                    "strategist",
                    &choice,
                    RoleCapability::Strategist,
                    &coord.tx,
                )
                .await;
            assert!(ov.is_explicit(), "a committed pair must be explicit");
            assert_eq!(tn.read, "cursor_read");
            assert_eq!(tn.write, "cursor_write");
            assert!(
                tn.toolset_tools.contains("cursor_read"),
                "explicit path must enumerate the toolset",
            );
            assert_eq!(inherit_tn.read, "read_file");
            assert_eq!(inherit_tn.write, "write");
        })
        .await;
}
/// InheritCurrent → default (inherit) override + parent-toolset tool_names with
/// an empty `{TOOLSET_TOOLS}` block; no committed pair, no leftover placeholder.
#[tokio::test(flavor = "current_thread")]
async fn single_role_override_inherit_uses_parent_tool_names() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(VecDeque::new());
            let (actor, _tmp) = make_role_model_actor(
                Some(coord.tx.clone()),
                1,
                Vec::new(),
                Vec::new(),
                false,
                indexmap::IndexMap::new(),
            )
            .await;
            let choice = crate::agent::config::GoalRoleModelChoice::InheritCurrent;
            let (ov, tn, _inherit_tn) = actor
                .resolve_goal_single_role_override(
                    "strategist",
                    &choice,
                    RoleCapability::Strategist,
                    &coord.tx,
                )
                .await;
            assert!(!ov.is_explicit(), "inherit choice must not commit a pair");
            assert_eq!(
                tn.toolset_tools, "",
                "the inherit path carries no toolset block",
            );
            let rendered = tn.apply("{READ_TOOL} {SEARCH_TOOL} {EXECUTE_TOOL}{TOOLSET_TOOLS}");
            assert!(
                !rendered.contains('{'),
                "no tool placeholder may survive the inherit render: {rendered}",
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn kill_switch_overrides_frozen_skeptic_assignment() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(VecDeque::from([
                Response::achieved(),
                Response::achieved(),
            ]));
            let pool = vec![
                role_pair("m0", "general-purpose"),
                role_pair("m1", "general-purpose"),
            ];
            let (actor, _tmp) = make_role_model_actor(
                Some(coord.tx.clone()),
                2,
                pool.clone(),
                pool,
                true,
                catalog_with(&[("m0", true), ("m1", true)]),
            )
            .await;
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let models = coord.spawn_models.lock().clone();
            assert!(!models.is_empty(), "skeptics must have spawned");
            assert!(
                models.iter().all(|m| m.is_none()),
                "kill-switch must force every skeptic to inherit despite the \
                 frozen assignment, got {models:?}",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn per_index_skeptic_models_reach_request_and_bad_index_degrades_alone() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(VecDeque::from([
                Response::achieved(),
                Response::achieved(),
            ]));
            let pool = vec![
                role_pair("ok-model", "general-purpose"),
                role_pair("blocked-model", "general-purpose"),
            ];
            let (actor, tmp) = make_role_model_actor(
                Some(coord.tx.clone()),
                2,
                pool,
                Vec::new(),
                false,
                catalog_with(&[("ok-model", true), ("blocked-model", false)]),
            )
            .await;
            seed_channel(&actor, vec![make_completed()]);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            let models = coord.spawn_models.lock().clone();
            assert_eq!(models.len(), 2, "two skeptics spawn: {models:?}");
            assert_eq!(
                models[0].as_deref(),
                Some("ok-model"),
                "entitled index 0 carries its configured model",
            );
            assert!(
                models[1].is_none(),
                "unauthorized index 1 degrades to inherit ONLY itself: {models:?}",
            );
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Complete),
                "a bad skeptic pair must not pause the goal",
            );
            drop(actor);
            let log = events_log(&tmp);
            let resolved = lines_with_type(&log, "goal_role_model_resolved");
            assert_eq!(resolved.len(), 1, "one committed index: {log}");
            assert_eq!(resolved[0]["skeptic_idx"], 0);
            let failed = lines_with_type(&log, "goal_role_model_fail_open");
            assert_eq!(failed.len(), 1, "one degraded index: {log}");
            assert_eq!(failed[0]["skeptic_idx"], 1);
            assert_eq!(failed[0]["reason"], "model_unauthorized");
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
/// A fail-open early-exit (here: traversal in `verifier_id` rejects the
/// details path before any spawn) must NOT overwrite the stored
/// gatekeeper id — only a stage that actually ran a panel may.
#[tokio::test(flavor = "current_thread")]
async fn stage_fail_open_preserves_stored_skeptic0_id() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(VecDeque::new());
            let (actor, _tmp) = make_actor(Some(coord.tx.clone()), true).await;
            {
                let mut tracker = actor.goal_tracker.lock();
                let o = tracker.snapshot_mut().unwrap();
                o.skeptic0_session_id = Some("prior-s0".into());
                o.verifier_id = "../etc".into();
            }
            let outcome = actor.run_verification_stage_for_drain(1, 3).await;
            assert!(
                matches!(
                    outcome,
                    crate::session::goal_classifier::GoalClassifierOutcome::FailOpenAchieved { .. },
                ),
                "test premise: the traversal verifier_id must fail open",
            );
            assert_eq!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .unwrap()
                    .skeptic0_session_id
                    .as_deref(),
                Some("prior-s0"),
                "fail-open must not sever the gatekeeper resume chain",
            );
            assert_eq!(coord.spawn_count.load(SeqOrd::SeqCst), 0, "no panel ran");
        })
        .await;
}
/// Counterpart: a panel that genuinely ran with N == 1 returns id `None`
/// and the apply call-site must clear the stored id (a resumed sole
/// judge would be the biased approver the design avoids).
#[tokio::test(flavor = "current_thread")]
async fn stage_n1_panel_run_clears_stored_skeptic0_id() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let coord = MockCoordinator::spawn(VecDeque::from([Response::not_achieved()]));
            let (actor, _tmp) = make_actor(Some(coord.tx.clone()), true).await;
            actor
                .goal_tracker
                .lock()
                .snapshot_mut()
                .unwrap()
                .skeptic0_session_id = Some("prior-s0".into());
            let _ = actor.run_verification_stage_for_drain(1, 3).await;
            assert_eq!(coord.spawn_count.load(SeqOrd::SeqCst), 1, "N==1 panel ran");
            assert!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .unwrap()
                    .skeptic0_session_id
                    .is_none(),
                "an N==1 panel run must clear the stale gatekeeper id",
            );
        })
        .await;
}
/// The first verification round captures the agent's full deliverable
/// summary, both sending it to the panel and freezing it on the tracker
/// as the breadth anchor for later rounds.
#[tokio::test(flavor = "current_thread")]
async fn first_verification_round_captures_full_summary_as_breadth_anchor() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            const SUMMARY: &str = "Full deliverable: built the parser, CLI, and 14 tests pass.";
            let coord = MockCoordinator::spawn(VecDeque::from([Response::not_achieved()]));
            let (actor, _tmp) = make_actor(Some(coord.tx.clone()), true).await;
            actor.chat_state_handle.push_assistant_response(
                xai_grok_sampling_types::conversation::ConversationItem::assistant(SUMMARY),
            );
            let _ = actor.run_verification_stage_for_drain(1, 3).await;
            assert_eq!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .unwrap()
                    .first_final_response
                    .as_deref(),
                Some(SUMMARY),
                "round 1 must freeze the full summary as the breadth anchor",
            );
            let prompts = coord.spawn_prompts.lock().clone();
            assert_eq!(prompts.len(), 1, "N==1 panel ran once: {prompts:?}");
            assert!(
                prompts[0].contains(SUMMARY),
                "round 1's panel must receive the full summary",
            );
        })
        .await;
}
/// On re-verification the stored anchor (round 1's full summary) is sent
/// to the cold panel ALONGSIDE this round's change note, and the anchor
/// is NOT overwritten — capture-once keeps breadth across rounds.
#[tokio::test(flavor = "current_thread")]
async fn reverification_replays_anchor_and_does_not_overwrite_it() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            const ANCHOR: &str = "Round 1 anchor: full deliverable summary across all modules.";
            const NOTE: &str = "Round 2: fixed the flagpole win bug; all 14 tests still pass.";
            let coord = MockCoordinator::spawn(VecDeque::from([Response::not_achieved()]));
            let (actor, _tmp) = make_actor(Some(coord.tx.clone()), true).await;
            actor
                .goal_tracker
                .lock()
                .snapshot_mut()
                .unwrap()
                .first_final_response = Some(ANCHOR.to_string());
            actor.chat_state_handle.push_assistant_response(
                xai_grok_sampling_types::conversation::ConversationItem::assistant(NOTE),
            );
            let _ = actor.run_verification_stage_for_drain(2, 3).await;
            let prompts = coord.spawn_prompts.lock().clone();
            assert_eq!(prompts.len(), 1, "N==1 panel ran once: {prompts:?}");
            assert!(
                prompts[0].contains(ANCHOR),
                "re-verification must keep the round-1 breadth anchor in the packet",
            );
            assert!(
                prompts[0].contains(NOTE) && prompts[0].contains("## Changes this round"),
                "re-verification must append this round's change note",
            );
            assert_eq!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .unwrap()
                    .first_final_response
                    .as_deref(),
                Some(ANCHOR),
                "capture-once: the anchor must not be overwritten on re-verification",
            );
        })
        .await;
}
/// The breadth anchor is frozen only AFTER the panel runs. A first
/// verification whose panel never ran (here a squatted scratch root forces
/// fail-open) must leave `first_final_response` uncaptured, so the next real
/// round still claims it.
#[tokio::test(flavor = "current_thread")]
async fn breadth_anchor_not_persisted_when_panel_does_not_run() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            const SUMMARY: &str = "Full deliverable: built everything; tests pass.";
            let coord = MockCoordinator::spawn(VecDeque::from([Response::not_achieved()]));
            let (actor, _tmp) = make_actor(Some(coord.tx.clone()), true).await;
            actor.chat_state_handle.push_assistant_response(
                xai_grok_sampling_types::conversation::ConversationItem::assistant(SUMMARY),
            );
            let vid = actor
                .goal_tracker
                .lock()
                .snapshot()
                .unwrap()
                .verifier_id
                .clone();
            let squatted_root = crate::session::goal_tracker::goal_scratch_root(&vid);
            let _ = std::fs::remove_dir_all(&squatted_root);
            std::fs::write(&squatted_root, b"squat").unwrap();
            let _ = actor.run_verification_stage_for_drain(1, 3).await;
            assert!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .unwrap()
                    .first_final_response
                    .is_none(),
                "anchor must not be frozen when the panel never ran",
            );
            let _ = std::fs::remove_file(&squatted_root);
        })
        .await;
}
