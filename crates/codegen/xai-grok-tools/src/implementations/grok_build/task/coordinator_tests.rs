use super::*;
use crate::implementations::grok_build::task::backend::{ChannelBackend, SubagentBackend};
use crate::implementations::grok_build::task::types::{
    SubagentCancelRequest, SubagentClearUsageNotAppliedRequest, SubagentCompletionsRequest,
    SubagentListActiveRequest, SubagentLoopUnitActiveRequest, SubagentMarkUsageNotAppliedRequest,
    SubagentOutstandingReply, SubagentOutstandingRequest, SubagentOwner, SubagentRegistryCounts,
    SubagentRequest, SubagentSnapshotStatus,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct TestControl {
    cancellation: CancellationToken,
}

impl ChildControl for TestControl {
    type ProgressFuture = std::future::Ready<SubagentProgress>;

    fn progress(&self) -> Self::ProgressFuture {
        std::future::ready(SubagentProgress {
            turn_count: 2,
            tool_call_count: 3,
            tokens_used: 100,
            context_window_tokens: 1_000,
            context_usage_pct: 10,
            tools_used: vec!["read_file".to_owned()],
            error_count: 0,
        })
    }

    fn cancel(&self) {
        self.cancellation.cancel();
    }
}

struct TestRunner {
    wait_before_start: bool,
    wait_after_cancel: bool,
    start: tokio::sync::broadcast::Sender<()>,
    finish: tokio::sync::broadcast::Sender<()>,
    completions: mpsc::UnboundedSender<CompletionDisposition>,
    requests: mpsc::UnboundedSender<SubagentRequest>,
    started: mpsc::UnboundedSender<String>,
}

impl ChildRunner for TestRunner {
    type Control = TestControl;
    type CompletionData = ();
    type RunFuture = SendBoxFuture<ChildRunOutput<()>>;
    type ValidateFuture = SendBoxFuture<SubagentValidateTypeOutcome>;
    type DescribeFuture = SendBoxFuture<SubagentDescribeOutcome>;

    fn run(&self, run: ChildRunRequest<Self::Control>) -> Self::RunFuture {
        let wait_before_start = self.wait_before_start;
        let wait_after_cancel = self.wait_after_cancel;
        let mut start = self.start.subscribe();
        let mut finish = self.finish.subscribe();
        let requests = self.requests.clone();
        let started = self.started.clone();
        Box::pin(async move {
            let ChildRunRequest {
                request,
                cancellation,
                reporter,
            } = run;
            let _ = requests.send(request.clone());
            if wait_before_start {
                tokio::select! {
                    _ = cancellation.cancelled() => {
                        if wait_after_cancel {
                            let _ = finish.recv().await;
                        }
                        return ChildRunOutput {
                            result: cancelled_result(&request),
                            completion_data: (),
                            snapshot_ref: None,
                        };
                    }
                    _ = start.recv() => {}
                }
            }
            if !reporter
                .started(StartedChild {
                    child_session_id: request.id.clone(),
                    persona: None,
                    resumed_from: request.resume_from.clone(),
                    child_cwd: request.cwd.clone().unwrap_or_default(),
                    worktree_path: None,
                    effective_model_id: "test-model".to_owned(),
                    // Mock definition resolution: this type declares background.
                    definition_background: request.subagent_type == "background-default",
                    control: TestControl {
                        cancellation: cancellation.clone(),
                    },
                })
                .await
            {
                return ChildRunOutput {
                    result: cancelled_result(&request),
                    completion_data: (),
                    snapshot_ref: None,
                };
            }
            let _ = started.send(request.id.clone());
            let result = tokio::select! {
                _ = cancellation.cancelled() => {
                    if wait_after_cancel {
                        let _ = finish.recv().await;
                    }
                    cancelled_result(&request)
                },
                _ = finish.recv() => SubagentResult {
                    success: true,
                    output: request.prompt.clone().into(),
                    subagent_id: request.id.clone(),
                    child_session_id: request.id.clone(),
                    tool_calls: 3,
                    turns: 2,
                    ..Default::default()
                },
            };
            ChildRunOutput {
                result,
                completion_data: (),
                snapshot_ref: None,
            }
        })
    }

    fn validate_type(
        &self,
        _subagent_type: String,
        _parent_session_id: String,
    ) -> Self::ValidateFuture {
        Box::pin(std::future::ready(SubagentValidateTypeOutcome::Ok))
    }

    fn describe_type(
        &self,
        _subagent_type: String,
        _harness_agent_type: Option<String>,
        _parent_session_id: String,
    ) -> Self::DescribeFuture {
        Box::pin(std::future::ready(SubagentDescribeOutcome::Unavailable))
    }

    fn on_completed(&self, completion: ChildCompletion<Self::CompletionData>) {
        let _ = self.completions.send(completion.disposition);
    }
}

fn cancelled_result(request: &SubagentRequest) -> SubagentResult {
    SubagentResult {
        success: false,
        cancelled: true,
        error: Some("cancelled".to_owned()),
        subagent_id: request.id.clone(),
        child_session_id: request.id.clone(),
        ..Default::default()
    }
}

fn request(id: &str, background: bool) -> SubagentRequest {
    SubagentRequest {
        id: id.to_owned(),
        prompt: "work".to_owned(),
        description: "test child".to_owned(),
        subagent_type: "explore".to_owned(),
        parent_session_id: "parent".to_owned(),
        parent_prompt_id: Some("prompt".to_owned()),
        resume_from: None,
        cwd: None,
        runtime_overrides: Default::default(),
        run_in_background: background,
        surface_completion: true,
        await_to_completion: false,
        fork_context: false,
        owner: SubagentOwner::Task,
        cancel_token: CancellationToken::new(),
    }
}

struct Harness {
    backend: ChannelBackend,
    start: tokio::sync::broadcast::Sender<()>,
    finish: tokio::sync::broadcast::Sender<()>,
    completions: mpsc::UnboundedReceiver<CompletionDisposition>,
    requests: mpsc::UnboundedReceiver<SubagentRequest>,
    started: mpsc::UnboundedReceiver<String>,
    actor: tokio::task::JoinHandle<()>,
}

fn harness(wait_before_start: bool, foreground_budget: std::time::Duration) -> Harness {
    harness_with_config(
        wait_before_start,
        CoordinatorConfig {
            foreground_budget,
            ..CoordinatorConfig::default()
        },
    )
}

fn harness_with_config(wait_before_start: bool, config: CoordinatorConfig) -> Harness {
    harness_with_options(wait_before_start, false, config)
}

fn harness_with_options(
    wait_before_start: bool,
    wait_after_cancel: bool,
    config: CoordinatorConfig,
) -> Harness {
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (start, _) = tokio::sync::broadcast::channel(4);
    let (finish, _) = tokio::sync::broadcast::channel(4);
    let (completion_tx, completions) = mpsc::unbounded_channel();
    let (request_tx, requests) = mpsc::unbounded_channel();
    let (started_tx, started) = mpsc::unbounded_channel();
    let actor = tokio::spawn(
        SubagentCoordinator::new(
            command_rx,
            TestRunner {
                wait_before_start,
                wait_after_cancel,
                start: start.clone(),
                finish: finish.clone(),
                completions: completion_tx,
                requests: request_tx,
                started: started_tx,
            },
            config,
        )
        .run(),
    );
    Harness {
        backend: ChannelBackend::new(command_tx),
        start,
        finish,
        completions,
        requests,
        started,
        actor,
    }
}

async fn loop_unit_active(backend: &ChannelBackend, task_id: &str) -> bool {
    let (respond_to, response_rx) = oneshot::channel();
    backend
        .sender()
        .send(SubagentEvent::LoopUnitActive(
            SubagentLoopUnitActiveRequest {
                task_id: task_id.to_owned(),
                respond_to,
            },
        ))
        .expect("actor command channel open");
    response_rx.await.expect("loop activity response")
}

async fn outstanding(backend: &ChannelBackend, prompt_id: &str) -> SubagentOutstandingReply {
    let (respond_to, response_rx) = oneshot::channel();
    backend
        .sender()
        .send(SubagentEvent::Outstanding(SubagentOutstandingRequest {
            parent_session_id: "parent".to_owned(),
            prompt_id: prompt_id.to_owned(),
            respond_to,
        }))
        .expect("actor command channel open");
    response_rx.await.expect("outstanding response")
}

#[tokio::test]
async fn foreground_completion_is_delivered_inline() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("inline", false)).await }
    });
    tokio::task::yield_now().await;
    let _ = harness.finish.send(());

    let result = spawn.await.unwrap().unwrap();
    assert!(result.success);
    let disposition = harness.completions.recv().await.unwrap();
    assert!(disposition.foreground_delivered);
    assert!(!disposition.should_surface);
    harness.actor.abort();
}

#[tokio::test(start_paused = true)]
async fn foreground_deadline_hands_off_without_stopping_child() {
    let mut harness = harness(false, std::time::Duration::from_secs(1));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("slow", false)).await }
    });
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    let interim = spawn.await.unwrap().unwrap();
    assert!(interim.backgrounded);
    // Interim handoff must not read as a completion (status() contract).
    assert!(!interim.success);
    assert_eq!(
        outstanding(&harness.backend, "prompt").await,
        SubagentOutstandingReply {
            live_ids: Vec::new(),
            background_live: true,
            subagent_usage_not_applied: false,
        }
    );
    assert_eq!(
        harness.backend.registry_counts().await,
        SubagentRegistryCounts {
            pending: 0,
            active: 1,
            completed: 0,
        }
    );

    let running = harness.backend.query("slow", false, None).await.unwrap();
    assert!(running.is_running());
    let _ = harness.finish.send(());
    let disposition = harness.completions.recv().await.unwrap();
    assert!(disposition.backgrounded);
    assert!(disposition.should_surface);
    harness.actor.abort();
}

#[tokio::test]
async fn live_blocking_waiter_suppresses_async_surface() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("waited", true)).await }
    });
    tokio::task::yield_now().await;
    let wait = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.query("waited", true, Some(60_000)).await }
    });
    tokio::task::yield_now().await;
    let _ = harness.finish.send(());

    assert!(wait.await.unwrap().unwrap().status.is_terminal());
    let disposition = harness.completions.recv().await.unwrap();
    assert!(disposition.waiter_delivered);
    assert!(!disposition.should_surface);
    assert!(spawn.await.unwrap().unwrap().success);
    harness.actor.abort();
}

#[tokio::test(start_paused = true)]
async fn timed_out_waiter_does_not_suppress_later_completion() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("timeout", true)).await }
    });
    tokio::task::yield_now().await;
    let snapshot = harness
        .backend
        .query("timeout", true, Some(1_000))
        .await
        .unwrap();
    assert!(snapshot.is_running());

    let _ = harness.finish.send(());
    let disposition = harness.completions.recv().await.unwrap();
    assert!(!disposition.waiter_delivered);
    assert!(disposition.should_surface);
    assert!(spawn.await.unwrap().unwrap().success);
    harness.actor.abort();
}

#[tokio::test(start_paused = true)]
async fn surviving_waiter_suppresses_after_peer_times_out() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("two-waiters", true)).await }
    });
    tokio::task::yield_now().await;
    let short = tokio::spawn({
        let backend = harness.backend.clone();
        async move {
            backend
                .query("two-waiters", true, Some(1_000))
                .await
                .unwrap()
        }
    });
    let long = tokio::spawn({
        let backend = harness.backend.clone();
        async move {
            backend
                .query("two-waiters", true, Some(60_000))
                .await
                .unwrap()
        }
    });
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    assert!(short.await.unwrap().is_running());

    let _ = harness.finish.send(());
    assert!(long.await.unwrap().status.is_terminal());
    let disposition = harness.completions.recv().await.unwrap();
    assert!(disposition.waiter_delivered);
    assert!(!disposition.should_surface);
    assert!(spawn.await.unwrap().unwrap().success);
    harness.actor.abort();
}

#[tokio::test]
async fn dropped_waiter_does_not_suppress_completion() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("dropped-wait", true)).await }
    });
    tokio::task::yield_now().await;
    let wait = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.query("dropped-wait", true, Some(60_000)).await }
    });
    tokio::task::yield_now().await;
    wait.abort();
    let _ = wait.await;

    let _ = harness.finish.send(());
    let disposition = harness.completions.recv().await.unwrap();
    assert!(!disposition.waiter_delivered);
    assert!(disposition.should_surface);
    assert!(spawn.await.unwrap().unwrap().success);
    harness.actor.abort();
}

#[tokio::test]
async fn pending_cancel_delivers_waiter_once() {
    let mut harness = harness(true, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("pending-cancel", true)).await }
    });
    tokio::task::yield_now().await;
    let wait = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.query("pending-cancel", true, Some(60_000)).await }
    });
    tokio::task::yield_now().await;
    assert!(matches!(
        harness.backend.cancel("pending-cancel").await,
        SubagentCancelOutcome::Cancelled
    ));
    let snapshot = wait.await.unwrap().unwrap();
    assert!(matches!(
        snapshot.status,
        SubagentSnapshotStatus::Cancelled { .. }
    ));
    let disposition = harness.completions.recv().await.unwrap();
    assert!(disposition.waiter_delivered);
    assert!(disposition.explicitly_killed);
    assert!(!disposition.should_surface);
    assert!(spawn.await.unwrap().unwrap().cancelled);
    harness.actor.abort();
}

#[tokio::test]
async fn caller_drop_during_initialization_does_not_drop_owned_run() {
    let mut harness = harness(true, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("owned", false)).await }
    });
    tokio::task::yield_now().await;
    spawn.abort();
    let _ = spawn.await;

    let initializing = harness.backend.query("owned", false, None).await.unwrap();
    assert!(matches!(
        initializing.status,
        SubagentSnapshotStatus::Initializing
    ));
    let _ = harness.start.send(());
    tokio::task::yield_now().await;
    let _ = harness.finish.send(());
    let disposition = harness.completions.recv().await.unwrap();
    assert!(
        disposition.should_surface,
        "dropped foreground receiver becomes handle-only"
    );
    let terminal = harness.backend.query("owned", false, None).await.unwrap();
    assert!(terminal.status.is_terminal());
    harness.actor.abort();
}

#[tokio::test]
async fn abandoned_foreground_caller_clears_outstanding() {
    // ParentGone parity: dropping the spawn await must leave Outstanding
    // (turn-freeze) without waiting for the foreground budget.
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("abandoned", false)).await }
    });
    tokio::task::yield_now().await;
    assert_eq!(
        outstanding(&harness.backend, "prompt").await.live_ids,
        vec!["abandoned".to_owned()],
        "live foreground child blocks the turn"
    );

    spawn.abort();
    let _ = spawn.await;
    assert_eq!(
        outstanding(&harness.backend, "prompt").await,
        SubagentOutstandingReply {
            live_ids: Vec::new(),
            background_live: true,
            subagent_usage_not_applied: false,
        },
        "caller-gone foreground is handle-only for Outstanding"
    );
    let running = harness
        .backend
        .query("abandoned", false, None)
        .await
        .unwrap();
    assert!(running.is_running(), "child keeps running after ParentGone");

    let _ = harness.finish.send(());
    let disposition = harness.completions.recv().await.unwrap();
    assert!(disposition.backgrounded);
    assert!(disposition.should_surface);
    harness.actor.abort();
}

#[tokio::test]
async fn duplicate_subagent_id_is_rejected_without_replacing_live_child() {
    let harness = harness(false, std::time::Duration::from_secs(60));
    let first = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("duplicate", true)).await }
    });
    tokio::task::yield_now().await;

    let duplicate = harness
        .backend
        .spawn(request("duplicate", false))
        .await
        .expect("duplicate rejection is a lifecycle result");
    assert!(!duplicate.success);
    assert!(
        duplicate
            .error
            .as_deref()
            .is_some_and(|error| error.contains("already exists"))
    );

    let running = harness
        .backend
        .query("duplicate", false, None)
        .await
        .expect("original child remains queryable");
    assert!(running.is_running());
    let _ = harness.finish.send(());
    assert!(first.await.unwrap().unwrap().success);
    harness.actor.abort();
}

#[tokio::test]
async fn external_cancel_token_cancels_live_child() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let request = request("external-cancel", false);
    let cancel_token = request.cancel_token.clone();
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request).await }
    });
    assert_eq!(
        harness.started.recv().await.as_deref(),
        Some("external-cancel")
    );

    cancel_token.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), spawn)
        .await
        .expect("external cancellation should finish")
        .unwrap()
        .unwrap();
    assert!(result.cancelled);
    let disposition = harness.completions.recv().await.unwrap();
    assert!(!disposition.explicitly_killed);
    harness.actor.abort();
}

#[tokio::test]
async fn dropping_coordinator_cancels_live_child() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let cancellation = CancellationToken::new();
    let mut request = request("owner-drop", true);
    request.cancel_token = cancellation.clone();
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("owner-drop"));

    harness.actor.abort();
    tokio::time::timeout(std::time::Duration::from_secs(1), cancellation.cancelled())
        .await
        .expect("coordinator drop should cancel child");
    assert!(spawn.await.unwrap().is_err());
}

#[tokio::test(start_paused = true)]
async fn await_to_completion_has_no_foreground_deadline() {
    let mut harness = harness(false, std::time::Duration::from_secs(1));
    let mut request = request("await-completion", false);
    request.await_to_completion = true;
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request).await }
    });
    assert_eq!(
        harness.started.recv().await.as_deref(),
        Some("await-completion")
    );

    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    assert!(!spawn.is_finished());
    let _ = harness.finish.send(());
    let result = spawn.await.unwrap().unwrap();
    assert!(result.success);
    assert!(!result.backgrounded);
    harness.actor.abort();
}

#[tokio::test]
async fn workflow_cancel_waits_for_drain_and_hides_owned_children() {
    let mut harness = harness_with_options(
        true,
        true,
        CoordinatorConfig {
            buffer_completions: true,
            ..CoordinatorConfig::default()
        },
    );

    let mut active_request = request("workflow-active", false);
    active_request.await_to_completion = true;
    active_request.owner = SubagentOwner::workflow("workflow-run");
    let active_spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(active_request).await }
    });
    assert_eq!(
        harness
            .requests
            .recv()
            .await
            .as_ref()
            .map(|request| request.id.as_str()),
        Some("workflow-active")
    );
    let _ = harness.start.send(());
    assert_eq!(
        harness.started.recv().await.as_deref(),
        Some("workflow-active")
    );

    let mut pending_request = request("workflow-pending", false);
    pending_request.await_to_completion = true;
    pending_request.owner = SubagentOwner::workflow("workflow-run");
    let pending_spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(pending_request).await }
    });
    assert_eq!(
        harness
            .requests
            .recv()
            .await
            .as_ref()
            .map(|request| request.id.as_str()),
        Some("workflow-pending")
    );

    assert!(
        harness
            .backend
            .query("workflow-active", false, None)
            .await
            .is_none()
    );
    assert!(
        harness
            .backend
            .query("workflow-pending", false, None)
            .await
            .is_none()
    );
    assert!(harness.backend.inspect("workflow-active").await.is_some());
    assert!(harness.backend.inspect("workflow-pending").await.is_some());
    assert!(harness.backend.list_running("parent").await.is_empty());
    let (list_respond_to, list_response_rx) = oneshot::channel();
    harness
        .backend
        .sender()
        .send(SubagentEvent::ListActive(SubagentListActiveRequest {
            parent_session_id: "parent".to_owned(),
            respond_to: list_respond_to,
        }))
        .expect("actor command channel open");
    assert!(list_response_rx.await.unwrap().is_empty());

    let (cancel_respond_to, mut cancel_response_rx) = oneshot::channel();
    harness
        .backend
        .sender()
        .send(SubagentEvent::Cancel(SubagentCancelRequest {
            parent_session_id: Some("parent".to_owned()),
            target: SubagentCancelTarget::WorkflowRunId("workflow-run".to_owned()),
            respond_to: cancel_respond_to,
        }))
        .expect("actor command channel open");
    assert!(harness.backend.inspect("workflow-active").await.is_some());
    assert!(matches!(
        cancel_response_rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));

    let _ = harness.finish.send(());
    assert!(matches!(
        cancel_response_rx.await.unwrap(),
        SubagentCancelOutcome::Cancelled
    ));
    assert!(active_spawn.await.unwrap().unwrap().cancelled);
    assert!(pending_spawn.await.unwrap().unwrap().cancelled);
    assert!(
        harness
            .backend
            .query("workflow-active", false, None)
            .await
            .is_none()
    );
    assert!(harness.backend.inspect("workflow-active").await.is_some());

    let (completions_respond_to, completions_response_rx) = oneshot::channel();
    harness
        .backend
        .sender()
        .send(SubagentEvent::Completions(SubagentCompletionsRequest {
            parent_session_id: Some("parent".to_owned()),
            suppress_ids: Vec::new(),
            respond_to: completions_respond_to,
        }))
        .expect("actor command channel open");
    assert!(completions_response_rx.await.unwrap().is_empty());
    harness.actor.abort();
}

#[tokio::test]
async fn usage_events_feed_sorted_outstanding_reply() {
    let mut harness = harness(true, std::time::Duration::from_secs(60));
    let mut spawns = Vec::new();
    for (id, is_background) in [
        ("z-foreground", false),
        ("a-foreground", false),
        ("background", true),
    ] {
        spawns.push(tokio::spawn({
            let backend = harness.backend.clone();
            async move { backend.spawn(request(id, is_background)).await }
        }));
        assert_eq!(
            harness
                .requests
                .recv()
                .await
                .as_ref()
                .map(|request| request.id.as_str()),
            Some(id)
        );
    }

    let (foreign_respond_to, foreign_response_rx) = oneshot::channel();
    harness
        .backend
        .sender()
        .send(SubagentEvent::MarkUsageNotApplied(
            SubagentMarkUsageNotAppliedRequest {
                parent_session_id: "foreign".to_owned(),
                prompt_id: "prompt".to_owned(),
                respond_to: foreign_respond_to,
            },
        ))
        .expect("actor command channel open");
    foreign_response_rx.await.expect("mark acknowledgement");
    assert!(
        !outstanding(&harness.backend, "prompt")
            .await
            .subagent_usage_not_applied
    );

    let (mark_respond_to, mark_response_rx) = oneshot::channel();
    harness
        .backend
        .sender()
        .send(SubagentEvent::MarkUsageNotApplied(
            SubagentMarkUsageNotAppliedRequest {
                parent_session_id: "parent".to_owned(),
                prompt_id: "prompt".to_owned(),
                respond_to: mark_respond_to,
            },
        ))
        .expect("actor command channel open");
    mark_response_rx.await.expect("mark acknowledgement");
    assert_eq!(
        outstanding(&harness.backend, "prompt").await,
        SubagentOutstandingReply {
            live_ids: vec!["a-foreground".to_owned(), "z-foreground".to_owned()],
            background_live: true,
            subagent_usage_not_applied: true,
        }
    );

    harness
        .backend
        .sender()
        .send(SubagentEvent::ClearUsageNotApplied(
            SubagentClearUsageNotAppliedRequest {
                parent_session_id: "parent".to_owned(),
                prompt_id: "prompt".to_owned(),
            },
        ))
        .expect("actor command channel open");
    assert_eq!(
        outstanding(&harness.backend, "prompt").await,
        SubagentOutstandingReply {
            live_ids: vec!["a-foreground".to_owned(), "z-foreground".to_owned()],
            background_live: true,
            subagent_usage_not_applied: false,
        }
    );

    assert!(matches!(
        harness.backend.cancel_parent_prompt("prompt").await,
        SubagentCancelOutcome::Cancelled
    ));
    for spawn in spawns {
        assert!(spawn.await.unwrap().unwrap().cancelled);
    }
    harness.actor.abort();
}

#[tokio::test]
async fn loop_tracking_covers_pending_active_and_nested_reparenting() {
    let mut harness = harness(true, std::time::Duration::from_secs(60));
    let mut outer_request = request("outer", true);
    outer_request.runtime_overrides.loop_task_id = Some("loop-task".to_owned());
    let outer_spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(outer_request).await }
    });
    let observed_outer = harness.requests.recv().await.unwrap();
    assert_eq!(observed_outer.parent_session_id, "parent");
    assert!(loop_unit_active(&harness.backend, "loop-task").await);

    let _ = harness.start.send(());
    assert_eq!(harness.started.recv().await.as_deref(), Some("outer"));
    let refs = harness
        .backend
        .spawned_refs_for_prompt("parent", "prompt")
        .await;
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].description, "test child");

    let mut nested_request = request("nested", true);
    nested_request.parent_session_id = "outer".to_owned();
    let nested_spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(nested_request).await }
    });
    let observed_nested = harness.requests.recv().await.unwrap();
    assert_eq!(observed_nested.parent_session_id, "parent");
    assert!(!observed_nested.surface_completion);
    assert_eq!(
        observed_nested.runtime_overrides.loop_task_id.as_deref(),
        Some("loop-task")
    );
    assert!(loop_unit_active(&harness.backend, "loop-task").await);

    let _ = harness.start.send(());
    assert_eq!(harness.started.recv().await.as_deref(), Some("nested"));
    let _ = harness.finish.send(());
    assert!(outer_spawn.await.unwrap().unwrap().success);
    assert!(nested_spawn.await.unwrap().unwrap().success);
    assert!(!loop_unit_active(&harness.backend, "loop-task").await);
    harness.actor.abort();
}

#[tokio::test]
async fn completion_buffer_caps_summary_without_mutating_result() {
    let mut harness = harness_with_config(
        false,
        CoordinatorConfig {
            buffer_completions: true,
            ..CoordinatorConfig::default()
        },
    );
    let mut request = request("buffered", true);
    request.prompt = "aéb".to_owned();
    request.runtime_overrides.completion_output_cap = Some(2);
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("buffered"));
    let _ = harness.finish.send(());
    let result = spawn.await.unwrap().unwrap();
    assert_eq!(result.output.as_ref(), "aéb");
    let _ = harness.completions.recv().await;
    let snapshot = harness
        .backend
        .query("buffered", false, None)
        .await
        .unwrap();
    let SubagentSnapshotStatus::Completed { output, .. } = snapshot.status else {
        panic!("expected completed snapshot");
    };
    assert_eq!(output, "aéb");

    let (respond_to, response_rx) = oneshot::channel();
    harness
        .backend
        .sender()
        .send(SubagentEvent::Completions(SubagentCompletionsRequest {
            parent_session_id: Some("parent".to_owned()),
            suppress_ids: Vec::new(),
            respond_to,
        }))
        .expect("actor command channel open");
    let buffered = response_rx.await.expect("completion response");
    assert_eq!(buffered.len(), 1);
    assert_eq!(buffered[0].subagent_id, "buffered");
    assert_eq!(
        buffered[0].output.as_ref(),
        "a\n[output truncated: 1 of 4 bytes shown]"
    );
    harness.actor.abort();
}

/// Regression (review): an agent definition with `background: true` spawned
/// with a BLOCKING tool call (`run_in_background: false`) is background for
/// Outstanding/freeze accounting — not turn-blocking — while the spawn caller
/// still receives the result inline.
#[tokio::test]
async fn definition_background_counts_as_background_for_outstanding() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let mut blocking_request = request("bg-def", false);
    blocking_request.subagent_type = "background-default".to_owned();
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(blocking_request).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("bg-def"));

    // Started with definition background: live for the child itself but not
    // turn-blocking; the prompt sees it as background work.
    assert_eq!(
        outstanding(&harness.backend, "prompt").await,
        SubagentOutstandingReply {
            live_ids: Vec::new(),
            background_live: true,
            subagent_usage_not_applied: false,
        }
    );

    // The blocking caller still gets the completed result inline.
    let _ = harness.finish.send(());
    let result = spawn.await.unwrap().unwrap();
    assert!(result.success);
    assert!(!result.backgrounded);
    harness.actor.abort();
}

#[tokio::test]
async fn buffered_completion_output_cap_bounds_buffered_summary() {
    let mut harness = harness_with_config(
        false,
        CoordinatorConfig {
            buffer_completions: true,
            buffered_completion_output_cap: Some(8),
            ..CoordinatorConfig::default()
        },
    );
    let mut request = request("capped", true);
    request.prompt = "x".repeat(64);
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("capped"));
    let _ = harness.finish.send(());
    // Spawn result and queryable snapshot keep the full output…
    let result = spawn.await.unwrap().unwrap();
    assert_eq!(result.output.len(), 64);
    let _ = harness.completions.recv().await;

    // …only the buffered reminder copy is truncated.
    let (respond_to, response_rx) = oneshot::channel();
    harness
        .backend
        .sender()
        .send(SubagentEvent::Completions(SubagentCompletionsRequest {
            parent_session_id: Some("parent".to_owned()),
            suppress_ids: Vec::new(),
            respond_to,
        }))
        .expect("actor command channel open");
    let buffered = response_rx.await.expect("completion response");
    assert_eq!(buffered.len(), 1);
    assert!(
        buffered[0]
            .output
            .contains("[output truncated: 8 of 64 bytes shown]"),
        "buffered output must be capped, got: {}",
        buffered[0].output
    );
    harness.actor.abort();
}

#[tokio::test]
async fn discard_session_completions_drops_only_that_sessions_buffer() {
    let mut harness = harness_with_config(
        false,
        CoordinatorConfig {
            buffer_completions: true,
            ..CoordinatorConfig::default()
        },
    );
    for (id, parent) in [("child-a", "parent-a"), ("child-b", "parent-b")] {
        let mut request = request(id, true);
        request.parent_session_id = parent.to_owned();
        let spawn = tokio::spawn({
            let backend = harness.backend.clone();
            async move { backend.spawn(request).await }
        });
        assert_eq!(harness.started.recv().await.as_deref(), Some(id));
        let _ = harness.finish.send(());
        assert!(spawn.await.unwrap().unwrap().success);
        let _ = harness.completions.recv().await;
    }

    // Removing parent-a (session unload) discards its buffered completion...
    harness
        .backend
        .sender()
        .send(SubagentEvent::DiscardSessionCompletions {
            parent_session_id: "parent-a".to_owned(),
        })
        .expect("actor command channel open");

    let drain = |parent: &str| {
        let sender = harness.backend.sender();
        let parent = parent.to_owned();
        async move {
            let (respond_to, response_rx) = oneshot::channel();
            sender
                .send(SubagentEvent::Completions(SubagentCompletionsRequest {
                    parent_session_id: Some(parent),
                    suppress_ids: Vec::new(),
                    respond_to,
                }))
                .expect("actor command channel open");
            response_rx.await.expect("completion response")
        }
    };
    assert!(drain("parent-a").await.is_empty());
    // ...while parent-b's completion stays buffered for its own drain.
    let b = drain("parent-b").await;
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].subagent_id, "child-b");
    harness.actor.abort();
}

#[tokio::test]
async fn completion_drain_is_scoped_to_parent_session() {
    let mut harness = harness_with_config(
        false,
        CoordinatorConfig {
            buffer_completions: true,
            ..CoordinatorConfig::default()
        },
    );
    for (id, parent) in [("child-a", "parent-a"), ("child-b", "parent-b")] {
        let mut request = request(id, true);
        request.parent_session_id = parent.to_owned();
        let spawn = tokio::spawn({
            let backend = harness.backend.clone();
            async move { backend.spawn(request).await }
        });
        assert_eq!(harness.started.recv().await.as_deref(), Some(id));
        let _ = harness.finish.send(());
        assert!(spawn.await.unwrap().unwrap().success);
        let _ = harness.completions.recv().await;
    }

    for (parent, expected_id) in [("parent-a", "child-a"), ("parent-b", "child-b")] {
        let (respond_to, response_rx) = oneshot::channel();
        harness
            .backend
            .sender()
            .send(SubagentEvent::Completions(SubagentCompletionsRequest {
                parent_session_id: Some(parent.to_owned()),
                suppress_ids: Vec::new(),
                respond_to,
            }))
            .expect("actor command channel open");
        let completions = response_rx.await.expect("completion response");
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].subagent_id, expected_id);
    }
    harness.actor.abort();
}

#[tokio::test]
async fn session_backend_cannot_query_or_cancel_foreign_child() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let spawn = tokio::spawn({
        let backend = harness.backend.clone();
        async move { backend.spawn(request("scoped", true)).await }
    });
    assert_eq!(harness.started.recv().await.as_deref(), Some("scoped"));

    let foreign = ChannelBackend::for_session(harness.backend.sender(), "foreign-parent");
    assert!(foreign.query("scoped", false, None).await.is_none());
    assert!(foreign.inspect("scoped").await.is_none());
    assert!(matches!(
        foreign.cancel("scoped").await,
        SubagentCancelOutcome::NotFound
    ));

    assert!(matches!(
        harness.backend.cancel("scoped").await,
        SubagentCancelOutcome::Cancelled
    ));
    assert!(spawn.await.unwrap().unwrap().cancelled);
    let _ = harness.completions.recv().await;
    harness.actor.abort();
}

#[tokio::test]
async fn completed_cache_evicts_oldest_entry_at_cap() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    for index in 0..=MAX_COMPLETED_ENTRIES {
        let id = format!("cache-{index:04}");
        let spawn = tokio::spawn({
            let backend = harness.backend.clone();
            let request = request(&id, true);
            async move { backend.spawn(request).await }
        });
        assert_eq!(harness.started.recv().await.as_deref(), Some(id.as_str()));
        let _ = harness.finish.send(());
        assert!(spawn.await.unwrap().unwrap().success);
    }

    assert!(
        harness
            .backend
            .query("cache-0000", false, None)
            .await
            .is_none()
    );
    assert!(
        harness
            .backend
            .query("cache-0001", false, None)
            .await
            .is_some()
    );
    assert!(
        harness
            .backend
            .query(&format!("cache-{MAX_COMPLETED_ENTRIES:04}"), false, None,)
            .await
            .is_some()
    );
    harness.actor.abort();
}
