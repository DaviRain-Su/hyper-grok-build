use std::future::Future;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::super::*;
use super::*;
use crate::implementations::grok_build::task::active_message::SubagentActiveMessageRequest;
use crate::implementations::grok_build::task::coordinator_state::{
    ACTIVE_MESSAGE_SPAWN_READY_TIMEOUT, InternalEvent, PendingChild,
};
use crate::implementations::grok_build::task::types::{
    ActiveAgentMessageOperation, ActiveAgentMessageRequest, SubagentResult,
};

const TEST_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

struct AdmissionCall {
    message_id: String,
    late_delivery: ActiveAgentMessageDelivery,
    release: oneshot::Sender<ActiveMessageAdmission>,
}

struct TestControl {
    admissions: mpsc::UnboundedSender<AdmissionCall>,
}

impl ChildControl for TestControl {
    type ProgressFuture = std::future::Ready<SubagentProgress>;

    fn progress(&self) -> Self::ProgressFuture {
        std::future::ready(SubagentProgress::default())
    }

    fn send_active_message(
        &self,
        delivery: ActiveAgentMessageDelivery,
    ) -> SendBoxFuture<ActiveMessageAdmission> {
        let (release, released) = oneshot::channel();
        let _ = self.admissions.send(AdmissionCall {
            message_id: delivery.message().message_id.clone(),
            late_delivery: delivery.clone(),
            release,
        });
        Box::pin(async move {
            let admission = released
                .await
                .unwrap_or(ActiveMessageAdmission::ChannelClosed);
            if admission == ActiveMessageAdmission::Admitted
                && delivery.commit_admission(|| ()).is_none()
            {
                return ActiveMessageAdmission::Rejected;
            }
            admission
        })
    }

    fn cancel(&self) {}
}

struct TestRunner;

struct PanickingRunner {
    admissions: mpsc::UnboundedSender<AdmissionCall>,
    started: mpsc::UnboundedSender<()>,
    panic: std::sync::Arc<tokio::sync::Notify>,
    completions: mpsc::UnboundedSender<SubagentResult>,
}

impl ChildRunner for PanickingRunner {
    type Control = TestControl;
    type CompletionData = ();
    type RunFuture = SendBoxFuture<ChildRunOutput<()>>;
    type ValidateFuture = SendBoxFuture<SubagentValidateTypeOutcome>;
    type DescribeFuture = SendBoxFuture<SubagentDescribeOutcome>;

    fn run(&self, run: ChildRunRequest<Self::Control>) -> Self::RunFuture {
        let admissions = self.admissions.clone();
        let started = self.started.clone();
        let panic = self.panic.clone();
        Box::pin(async move {
            let request = run.request;
            assert!(
                run.reporter
                    .started(StartedChild {
                        child_session_id: request.id.clone(),
                        persona: None,
                        resumed_from: None,
                        child_cwd: String::new(),
                        worktree_path: None,
                        effective_model_id: "test-model".to_owned(),
                        definition_background: false,
                        control: TestControl { admissions },
                    })
                    .await
            );
            let _ = started.send(());
            panic.notified().await;
            panic!("runner panic after promotion");
        })
    }

    fn validate_type(&self, _: String, _: String) -> Self::ValidateFuture {
        Box::pin(std::future::pending())
    }

    fn describe_type(&self, _: String, _: Option<String>, _: String) -> Self::DescribeFuture {
        Box::pin(std::future::pending())
    }

    fn on_completed(&self, completion: ChildCompletion<()>) {
        let _ = self.completions.send(completion.result);
    }
}

impl ChildRunner for TestRunner {
    type Control = TestControl;
    type CompletionData = ();
    type RunFuture = SendBoxFuture<ChildRunOutput<()>>;
    type ValidateFuture = SendBoxFuture<SubagentValidateTypeOutcome>;
    type DescribeFuture = SendBoxFuture<SubagentDescribeOutcome>;

    fn run(&self, _: ChildRunRequest<Self::Control>) -> Self::RunFuture {
        Box::pin(std::future::pending())
    }

    fn validate_type(&self, _: String, _: String) -> Self::ValidateFuture {
        Box::pin(std::future::pending())
    }

    fn describe_type(&self, _: String, _: Option<String>, _: String) -> Self::DescribeFuture {
        Box::pin(std::future::pending())
    }

    fn on_completed(&self, _: ChildCompletion<()>) {}
}

type TestCoordinator = SubagentCoordinator<TestRunner>;

fn fixture_with_capacity(
    active_message_capacity: usize,
) -> (
    TestCoordinator,
    crate::implementations::grok_build::task::backend::SubagentCoordinatorSender,
    mpsc::UnboundedSender<AdmissionCall>,
    mpsc::UnboundedReceiver<AdmissionCall>,
) {
    let config = CoordinatorConfig::default();
    let (command_tx, command_rx) =
        SubagentCoordinatorReceiver::with_capacity(active_message_capacity);
    let (admission_tx, admissions) = mpsc::unbounded_channel();
    let coordinator = SubagentCoordinator::from_channel(command_rx, TestRunner, config);
    (coordinator, command_tx, admission_tx, admissions)
}

fn fixture() -> (
    TestCoordinator,
    crate::implementations::grok_build::task::backend::SubagentCoordinatorSender,
    mpsc::UnboundedSender<AdmissionCall>,
    mpsc::UnboundedReceiver<AdmissionCall>,
) {
    fixture_with_capacity(MAX_ACTIVE_MESSAGE_ADMISSIONS)
}

fn insert_child(
    coordinator: &mut TestCoordinator,
    admissions: mpsc::UnboundedSender<AdmissionCall>,
    id: &str,
    parent: &str,
) {
    let mut request =
        crate::implementations::grok_build::task::coordinator::tests::request(id, true);
    request.parent_session_id = parent.to_owned();
    request.surface_completion = false;
    coordinator.active.insert(
        id.to_owned(),
        ActiveChild {
            request,
            started_at: std::time::Instant::now(),
            cancellation: CancellationToken::new(),
            spawn_reply: None,
            foreground_deadline: None,
            handle_only: true,
            definition_background: false,
            explicitly_killed: false,
            child_session_id: id.to_owned(),
            persona: None,
            resumed_from: None,
            child_cwd: String::new(),
            worktree_path: None,
            effective_model_id: "test-model".to_owned(),
            generation: ActiveChildGeneration::new(),
            active_messages: ActiveMessageLifecycle::default(),
            control: TestControl { admissions },
        },
    );
}

fn insert_pending(coordinator: &mut TestCoordinator, id: &str, parent: &str) {
    let mut request =
        crate::implementations::grok_build::task::coordinator::tests::request(id, true);
    request.parent_session_id = parent.to_owned();
    request.surface_completion = false;
    coordinator.pending.insert(
        id.to_owned(),
        PendingChild {
            request,
            started_at: std::time::Instant::now(),
            cancellation: CancellationToken::new(),
            spawn_reply: None,
            foreground_deadline: None,
            handle_only: true,
            explicitly_killed: false,
            launched: true,
        },
    );
}

fn promote_pending(
    coordinator: &mut TestCoordinator,
    admissions: mpsc::UnboundedSender<AdmissionCall>,
    id: &str,
) {
    let (respond_to, _response) = oneshot::channel();
    coordinator.handle_internal(InternalEvent::Started {
        subagent_id: id.to_owned(),
        child: StartedChild {
            child_session_id: id.to_owned(),
            persona: None,
            resumed_from: None,
            child_cwd: String::new(),
            worktree_path: None,
            effective_model_id: "test-model".to_owned(),
            definition_background: false,
            control: TestControl { admissions },
        },
        respond_to,
    });
}

fn begin_send(
    coordinator: &mut TestCoordinator,
    command_tx: &crate::implementations::grok_build::task::backend::SubagentCoordinatorSender,
    id: &str,
    parent: &str,
) -> oneshot::Receiver<ActiveAgentMessageOutcome> {
    let (respond_to, response_rx) = oneshot::channel();
    let request = SubagentActiveMessageRequest {
        request: ActiveAgentMessageRequest::try_new(id, "follow up").unwrap(),
        parent_session_id: parent.to_owned(),
        respond_to,
    };
    command_tx
        .try_send_active_message(request)
        .expect("active-message ingress open");
    let ingress = coordinator
        .active_message_ingress
        .as_mut()
        .expect("paired active-message ingress")
        .try_recv()
        .expect("active-message command queued");
    coordinator.handle_send_active_message(ingress);
    response_rx
}

async fn await_with_timeout<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(TEST_WAIT, future)
        .await
        .expect("active-message test wait timed out")
}

async fn recv_with_timeout<T>(receiver: &mut mpsc::UnboundedReceiver<T>) -> T {
    await_with_timeout(receiver.recv())
        .await
        .expect("active-message test channel closed")
}

async fn finish_next_active_message(coordinator: &mut TestCoordinator) {
    let completion = await_with_timeout(coordinator.active_messages.next())
        .await
        .expect("active-message completion stream ended");
    coordinator.finish_active_message(completion);
}

async fn release_admission(
    coordinator: &mut TestCoordinator,
    call: AdmissionCall,
    admission: ActiveMessageAdmission,
) {
    call.release
        .send(admission)
        .expect("active-message admission future dropped");
    finish_next_active_message(coordinator).await;
}

async fn response_outcome(
    response: oneshot::Receiver<ActiveAgentMessageOutcome>,
) -> ActiveAgentMessageOutcome {
    await_with_timeout(response)
        .await
        .expect("active-message response dropped")
}

async fn response_outcome_result(response: oneshot::Receiver<SubagentResult>) -> SubagentResult {
    await_with_timeout(response)
        .await
        .expect("spawn result dropped")
}

async fn finalization_outcome(response: oneshot::Receiver<bool>) -> bool {
    await_with_timeout(response)
        .await
        .expect("active-message finalization response dropped")
}

fn finish_child(coordinator: &mut TestCoordinator, id: &str) {
    coordinator.begin_terminalization(
        id,
        ChildRunOutput {
            result: SubagentResult {
                success: true,
                subagent_id: id.to_owned(),
                child_session_id: id.to_owned(),
                ..Default::default()
            },
            completion_data: (),
            snapshot_ref: None,
        },
    );
}

#[tokio::test]
async fn legacy_public_active_message_event_fails_closed() {
    let (tx, rx) = mpsc::unbounded_channel();
    let coordinator = SubagentCoordinator::new(rx, TestRunner, CoordinatorConfig::default());
    let actor = tokio::spawn(coordinator.run());
    let (respond_to, response) = oneshot::channel();
    tx.send(
        crate::implementations::grok_build::task::types::SubagentEvent::SendActiveMessage(
            SubagentActiveMessageRequest {
                request: ActiveAgentMessageRequest::try_new("child", "follow up").unwrap(),
                parent_session_id: "parent".to_owned(),
                respond_to,
            },
        ),
    )
    .unwrap();
    assert_eq!(
        ActiveAgentMessageOutcome::Unsupported,
        response_outcome(response).await
    );
    drop(tx);
    await_with_timeout(actor).await.unwrap();
}

#[tokio::test]
async fn two_sequential_admissions_keep_child_open() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx, "child", "parent");
    for _ in 0..2 {
        let (respond_to, response) = oneshot::channel();
        let request = SubagentActiveMessageRequest {
            request: ActiveAgentMessageRequest::try_new_with_operation(
                "child",
                "follow up",
                ActiveAgentMessageOperation::Steer,
            )
            .unwrap(),
            parent_session_id: "parent".to_owned(),
            respond_to,
        };
        command_tx
            .try_send_active_message(request)
            .expect("active-message ingress open");
        let ingress = coordinator
            .active_message_ingress
            .as_mut()
            .expect("paired active-message ingress")
            .try_recv()
            .expect("active-message command queued");
        coordinator.handle_send_active_message(ingress);

        let call = recv_with_timeout(&mut admissions).await;
        let message_id = call.message_id.clone();
        assert_eq!(
            call.late_delivery.operation(),
            ActiveAgentMessageOperation::Steer
        );
        release_admission(&mut coordinator, call, ActiveMessageAdmission::Admitted).await;
        assert_eq!(
            ActiveAgentMessageOutcome::Accepted { message_id },
            response_outcome(response).await
        );
    }
}

#[tokio::test]
async fn finalizing_first_and_send_first_are_deterministic() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx.clone(), "closed", "parent");
    let (closed_tx, closed_rx) = oneshot::channel();
    coordinator.handle_active_message_finalizing("closed".to_owned(), closed_tx);
    assert!(finalization_outcome(closed_rx).await);
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(begin_send(
            &mut coordinator,
            &command_tx,
            "closed",
            "parent"
        ))
        .await
    );
    assert!(admissions.try_recv().is_err());

    insert_child(&mut coordinator, admission_tx, "held", "parent");
    let admitted = begin_send(&mut coordinator, &command_tx, "held", "parent");
    let call = recv_with_timeout(&mut admissions).await;
    let (finalize_tx, mut finalize_rx) = oneshot::channel();
    coordinator.handle_active_message_finalizing("held".to_owned(), finalize_tx);
    assert!(finalize_rx.try_recv().is_err());
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(begin_send(&mut coordinator, &command_tx, "held", "parent")).await
    );
    assert!(admissions.try_recv().is_err());
    release_admission(&mut coordinator, call, ActiveMessageAdmission::Admitted).await;
    assert!(matches!(
        response_outcome(admitted).await,
        ActiveAgentMessageOutcome::Accepted { .. }
    ));
    assert!(finalization_outcome(finalize_rx).await);
}

#[tokio::test]
async fn finalization_waits_for_both_admissions_on_one_child() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx, "held", "parent");

    let first_response = begin_send(&mut coordinator, &command_tx, "held", "parent");
    let first_call = recv_with_timeout(&mut admissions).await;
    let second_response = begin_send(&mut coordinator, &command_tx, "held", "parent");
    let second_call = recv_with_timeout(&mut admissions).await;
    let (finalize_tx, mut finalize_rx) = oneshot::channel();
    coordinator.handle_active_message_finalizing("held".to_owned(), finalize_tx);

    release_admission(
        &mut coordinator,
        first_call,
        ActiveMessageAdmission::Rejected,
    )
    .await;
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(first_response).await
    );
    assert!(finalize_rx.try_recv().is_err());

    release_admission(
        &mut coordinator,
        second_call,
        ActiveMessageAdmission::Rejected,
    )
    .await;
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(second_response).await
    );
    assert!(finalization_outcome(finalize_rx).await);
}

#[tokio::test]
async fn admission_results_release_lifecycle_accounting() {
    let results = [
        (
            ActiveMessageAdmission::Unsupported,
            ActiveAgentMessageOutcome::Unsupported,
        ),
        (
            ActiveMessageAdmission::ChannelClosed,
            ActiveAgentMessageOutcome::ChannelClosed,
        ),
        (
            ActiveMessageAdmission::Rejected,
            ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        ),
    ];
    for (admission, expected) in results {
        let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
        insert_child(&mut coordinator, admission_tx, "child", "parent");
        let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
        let call = recv_with_timeout(&mut admissions).await;
        let (finalize_tx, finalize_rx) = oneshot::channel();
        coordinator.handle_active_message_finalizing("child".to_owned(), finalize_tx);

        release_admission(&mut coordinator, call, admission).await;
        assert_eq!(expected, response_outcome(response).await);
        assert!(finalization_outcome(finalize_rx).await);
    }
}

#[tokio::test]
async fn per_child_admission_cap_rejects_before_invoking_host() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx, "child", "parent");

    let mut responses = Vec::new();
    let mut calls = Vec::new();
    for _ in 0..MAX_ACTIVE_MESSAGE_ADMISSIONS_PER_CHILD {
        responses.push(begin_send(&mut coordinator, &command_tx, "child", "parent"));
        calls.push(recv_with_timeout(&mut admissions).await);
    }

    assert_eq!(
        ActiveAgentMessageOutcome::Saturated {
            max_in_flight: MAX_ACTIVE_MESSAGE_ADMISSIONS_PER_CHILD,
        },
        response_outcome(begin_send(&mut coordinator, &command_tx, "child", "parent")).await
    );
    assert!(admissions.try_recv().is_err());

    for (response, call) in responses.into_iter().zip(calls) {
        release_admission(&mut coordinator, call, ActiveMessageAdmission::Rejected).await;
        assert_eq!(
            ActiveAgentMessageOutcome::NotActiveOrFinalizing,
            response_outcome(response).await
        );
    }
}

#[tokio::test]
async fn global_ingress_cap_rejects_before_enqueue_and_drains() {
    const CAPACITY: usize = 3;
    let (mut coordinator, command_tx, admission_tx, mut admissions) =
        fixture_with_capacity(CAPACITY);
    for id in ["first", "second", "third", "overflow"] {
        insert_child(&mut coordinator, admission_tx.clone(), id, "parent");
    }

    let mut responses = Vec::new();
    let mut calls = Vec::new();
    for id in ["first", "second", "third"] {
        responses.push(begin_send(&mut coordinator, &command_tx, id, "parent"));
        calls.push(recv_with_timeout(&mut admissions).await);
    }
    let (respond_to, overflow_response) = oneshot::channel();
    assert_eq!(
        Err(CAPACITY),
        command_tx.try_send_active_message(SubagentActiveMessageRequest {
            request: ActiveAgentMessageRequest::try_new("overflow", "follow up").unwrap(),
            parent_session_id: "parent".to_owned(),
            respond_to,
        })
    );
    assert!(await_with_timeout(overflow_response).await.is_err());
    assert!(admissions.try_recv().is_err());

    for (response, call) in responses.into_iter().zip(calls) {
        release_admission(&mut coordinator, call, ActiveMessageAdmission::Rejected).await;
        assert_eq!(
            ActiveAgentMessageOutcome::NotActiveOrFinalizing,
            response_outcome(response).await
        );
    }
    let (respond_to, response) = oneshot::channel();
    assert!(
        command_tx
            .try_send_active_message(SubagentActiveMessageRequest {
                request: ActiveAgentMessageRequest::try_new("overflow", "follow up").unwrap(),
                parent_session_id: "parent".to_owned(),
                respond_to,
            })
            .is_ok()
    );
    drop(coordinator);
    assert!(await_with_timeout(response).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn admission_deadline_revocation_is_definite_and_unblocks_finalization() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx, "child", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    let held = recv_with_timeout(&mut admissions).await;
    let (finalize_tx, mut finalize_rx) = oneshot::channel();
    coordinator.handle_active_message_finalizing("child".to_owned(), finalize_tx);
    assert!(finalize_rx.try_recv().is_err());

    tokio::time::advance(ACTIVE_MESSAGE_ADMISSION_TIMEOUT).await;
    finish_next_active_message(&mut coordinator).await;
    assert_eq!(
        ActiveAgentMessageOutcome::NotAcceptedBeforeDeadline,
        response_outcome(response).await
    );
    assert!(finalization_outcome(finalize_rx).await);
    assert!(held.release.send(ActiveMessageAdmission::Admitted).is_err());
    assert!(held.late_delivery.commit_admission(|| ()).is_none());
    assert_eq!(coordinator.active_messages.len(), 0);
}

#[tokio::test]
async fn cancellation_revocation_is_definite_and_settles_finalization() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx, "child", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    let held = recv_with_timeout(&mut admissions).await;
    let (finalize_tx, finalize_rx) = oneshot::channel();
    coordinator.handle_active_message_finalizing("child".to_owned(), finalize_tx);
    coordinator
        .active
        .get("child")
        .expect("active child")
        .cancellation
        .cancel();

    finish_next_active_message(&mut coordinator).await;
    assert_eq!(
        ActiveAgentMessageOutcome::NotAcceptedBeforeDeadline,
        response_outcome(response).await
    );
    assert!(finalization_outcome(finalize_rx).await);
    assert!(held.release.send(ActiveMessageAdmission::Admitted).is_err());
    assert!(held.late_delivery.commit_admission(|| ()).is_none());
    assert_eq!(coordinator.active_messages.len(), 0);
}

#[tokio::test(start_paused = true)]
async fn claimed_deadline_race_remains_uncertain_and_finalization_unclean() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx, "child", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    let held = recv_with_timeout(&mut admissions).await;
    held.late_delivery.mark_admission_uncertain();
    let (finalize_tx, finalize_rx) = oneshot::channel();
    coordinator.handle_active_message_finalizing("child".to_owned(), finalize_tx);

    tokio::time::advance(ACTIVE_MESSAGE_ADMISSION_TIMEOUT).await;
    finish_next_active_message(&mut coordinator).await;

    assert_eq!(
        ActiveAgentMessageOutcome::AdmissionUncertain,
        response_outcome(response).await
    );
    assert!(!finalization_outcome(finalize_rx).await);
}

#[tokio::test]
async fn unsettled_completion_before_runner_output_marks_terminal_result_failed() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx, "child", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    let call = recv_with_timeout(&mut admissions).await;
    call.late_delivery.mark_admission_uncertain();
    coordinator
        .active
        .get("child")
        .expect("active child")
        .cancellation
        .cancel();
    finish_next_active_message(&mut coordinator).await;
    assert_eq!(
        ActiveAgentMessageOutcome::AdmissionUncertain,
        response_outcome(response).await
    );
    finish_child(&mut coordinator, "child");
    let result = &coordinator
        .completed
        .get("child")
        .expect("completed")
        .result;
    assert!(!result.success);
    assert!(result.cancelled);
}

#[tokio::test]
async fn runner_output_parked_before_unsettled_completion_is_failed() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx, "child", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    let call = recv_with_timeout(&mut admissions).await;
    call.late_delivery.mark_admission_uncertain();

    finish_child(&mut coordinator, "child");
    assert!(coordinator.terminal_outputs.contains_key("child"));
    coordinator
        .active
        .get("child")
        .expect("active child")
        .cancellation
        .cancel();
    finish_next_active_message(&mut coordinator).await;
    assert_eq!(
        ActiveAgentMessageOutcome::AdmissionUncertain,
        response_outcome(response).await
    );
    let result = &coordinator
        .completed
        .get("child")
        .expect("completed")
        .result;
    assert!(!result.success);
    assert!(result.cancelled);
}

#[tokio::test]
async fn runner_panic_parks_until_uncertain_admission_terminalizes_failed() {
    let (command_tx, command_rx) = SubagentCoordinatorReceiver::with_capacity(1);
    let (admission_tx, mut admissions) = mpsc::unbounded_channel();
    let (started_tx, mut started) = mpsc::unbounded_channel();
    let panic = std::sync::Arc::new(tokio::sync::Notify::new());
    let (completion_tx, mut completions) = mpsc::unbounded_channel();
    let actor = tokio::spawn(
        SubagentCoordinator::from_channel(
            command_rx,
            PanickingRunner {
                admissions: admission_tx,
                started: started_tx,
                panic: panic.clone(),
                completions: completion_tx,
            },
            CoordinatorConfig::default(),
        )
        .run(),
    );
    let backend =
        crate::implementations::grok_build::task::backend::ChannelBackend::for_coordinator_session(
            command_tx, "parent",
        );
    let mut spawn = tokio::spawn({
        let backend = backend.clone();
        async move {
            let mut request = crate::implementations::grok_build::task::coordinator::tests::request(
                "panic-child",
                false,
            );
            request.parent_session_id = "parent".to_owned();
            crate::implementations::grok_build::task::backend::SubagentBackend::spawn(
                &backend, request, None,
            )
            .await
        }
    });
    recv_with_timeout(&mut started).await;
    let response = tokio::spawn({
        let backend = backend.clone();
        async move {
            crate::implementations::grok_build::task::backend::SubagentBackend::send_active_message(
                &backend,
                ActiveAgentMessageRequest::try_new("panic-child", "follow up").unwrap(),
            )
            .await
        }
    });
    let call = recv_with_timeout(&mut admissions).await;
    call.late_delivery.mark_admission_uncertain();

    panic.notify_one();
    let short_wait = std::time::Duration::from_millis(100);
    assert!(
        tokio::time::timeout(short_wait, completions.recv())
            .await
            .is_err()
    );
    assert!(tokio::time::timeout(short_wait, &mut spawn).await.is_err());

    call.release
        .send(ActiveMessageAdmission::Rejected)
        .expect("selected admission future alive");
    assert_eq!(
        ActiveAgentMessageOutcome::AdmissionUncertain,
        await_with_timeout(response).await.unwrap()
    );
    let result = recv_with_timeout(&mut completions).await;
    assert!(!result.success);
    assert!(result.cancelled);
    assert_eq!(Some("Subagent runtime panicked"), result.error.as_deref());
    let spawned_result = await_with_timeout(&mut spawn).await.unwrap().unwrap();
    assert!(!spawned_result.success);
    assert!(spawned_result.cancelled);
    drop(backend);
    actor.abort();
    let _ = await_with_timeout(actor).await;
}

#[tokio::test]
async fn coordinator_terminalization_parks_output_until_admission_settles() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx, "child", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    let call = recv_with_timeout(&mut admissions).await;

    finish_child(&mut coordinator, "child");
    assert!(coordinator.active.contains_key("child"));
    assert!(coordinator.terminal_outputs.contains_key("child"));
    assert!(!coordinator.completed.contains_key("child"));

    release_admission(&mut coordinator, call, ActiveMessageAdmission::Rejected).await;
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(response).await
    );
    assert!(!coordinator.active.contains_key("child"));
    assert!(!coordinator.terminal_outputs.contains_key("child"));
    assert!(coordinator.completed.contains_key("child"));
}

#[tokio::test]
async fn stale_settled_rejection_remains_definite() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx.clone(), "reused", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "reused", "parent");
    let call = recv_with_timeout(&mut admissions).await;
    finish_child(&mut coordinator, "reused");
    coordinator.completed.remove("reused");
    coordinator.completed_order.clear();
    insert_child(&mut coordinator, admission_tx, "reused", "parent");

    release_admission(&mut coordinator, call, ActiveMessageAdmission::Rejected).await;

    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(response).await
    );
}

#[tokio::test]
async fn stale_admission_and_completed_lookup_preserve_terminal_authority() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx.clone(), "reused", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "reused", "parent");
    let call = recv_with_timeout(&mut admissions).await;
    finish_child(&mut coordinator, "reused");
    coordinator.completed.remove("reused");
    coordinator.completed_order.clear();
    insert_child(&mut coordinator, admission_tx.clone(), "reused", "parent");
    let late_delivery = call.late_delivery.clone();
    release_admission(&mut coordinator, call, ActiveMessageAdmission::Admitted).await;
    assert_eq!(
        ActiveAgentMessageOutcome::AdmissionUncertain,
        response_outcome(response).await
    );
    assert!(late_delivery.commit_admission(|| ()).is_none());

    insert_child(&mut coordinator, admission_tx, "completed", "parent");
    finish_child(&mut coordinator, "completed");
    for id in ["completed", "missing"] {
        assert_eq!(
            ActiveAgentMessageOutcome::NotFoundOrNotOwned,
            response_outcome(begin_send(&mut coordinator, &command_tx, id, "foreign")).await
        );
    }
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(begin_send(
            &mut coordinator,
            &command_tx,
            "completed",
            "parent"
        ))
        .await
    );
}

#[tokio::test]
async fn actor_drop_classifies_channel_closure_from_lease_proof() {
    for (is_claimed, expected) in [
        (false, ActiveAgentMessageOutcome::ChannelClosed),
        (true, ActiveAgentMessageOutcome::AdmissionUncertain),
    ] {
        let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
        insert_child(&mut coordinator, admission_tx, "child", "parent");
        let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
        let held = recv_with_timeout(&mut admissions).await;
        if is_claimed {
            held.late_delivery.mark_admission_uncertain();
        }

        drop(coordinator);
        assert_eq!(expected, response_outcome(response).await);
        assert!(held.late_delivery.commit_admission(|| ()).is_none());
    }
}

#[tokio::test]
async fn dropped_pre_commit_completion_is_channel_closed() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx, "child", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    let call = recv_with_timeout(&mut admissions).await;
    call.release
        .send(ActiveMessageAdmission::ChannelClosed)
        .expect("admission waiter");
    let completion = await_with_timeout(coordinator.active_messages.next())
        .await
        .expect("completion");
    assert!(completion.is_settled);
    drop(completion);
    assert_eq!(
        ActiveAgentMessageOutcome::ChannelClosed,
        response_outcome(response).await
    );
}

#[tokio::test]
async fn dropped_admitted_completion_is_uncertain_and_finalization_unclean() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx, "child", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    let call = recv_with_timeout(&mut admissions).await;
    call.release
        .send(ActiveMessageAdmission::Admitted)
        .expect("admission waiter");
    let completion = await_with_timeout(coordinator.active_messages.next())
        .await
        .expect("completion");
    assert!(completion.is_settled);
    drop(completion);
    assert_eq!(
        ActiveAgentMessageOutcome::AdmissionUncertain,
        response_outcome(response).await
    );
    let (finalize_tx, mut finalize_rx) = oneshot::channel();
    coordinator.handle_active_message_finalizing("child".to_owned(), finalize_tx);
    assert!(
        finalize_rx.try_recv().is_err(),
        "lost admitted completion must leave finalization in-flight"
    );
    drop(coordinator);
    assert!(!finalization_outcome(finalize_rx).await);
}

#[tokio::test]
async fn held_admission_is_independent_per_child() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_child(&mut coordinator, admission_tx.clone(), "first", "parent");
    insert_child(&mut coordinator, admission_tx, "second", "parent");
    let first_response = begin_send(&mut coordinator, &command_tx, "first", "parent");
    let first_call = recv_with_timeout(&mut admissions).await;
    let (first_tx, mut first_rx) = oneshot::channel();
    coordinator.handle_active_message_finalizing("first".to_owned(), first_tx);
    let (second_tx, second_rx) = oneshot::channel();
    coordinator.handle_active_message_finalizing("second".to_owned(), second_tx);
    assert!(finalization_outcome(second_rx).await);
    assert!(first_rx.try_recv().is_err());
    release_admission(
        &mut coordinator,
        first_call,
        ActiveMessageAdmission::Rejected,
    )
    .await;
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(first_response).await
    );
    assert!(finalization_outcome(first_rx).await);
}

#[tokio::test]
async fn send_to_owned_pending_waits_until_started_then_admits() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_pending(&mut coordinator, "child", "parent");
    let mut response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    assert!(response.try_recv().is_err());

    promote_pending(&mut coordinator, admission_tx, "child");
    let call = recv_with_timeout(&mut admissions).await;
    let message_id = call.message_id.clone();
    release_admission(&mut coordinator, call, ActiveMessageAdmission::Admitted).await;
    assert_eq!(
        ActiveAgentMessageOutcome::Accepted { message_id },
        response_outcome(response).await
    );
}

#[tokio::test]
async fn send_to_owned_pending_that_fails_is_not_active() {
    let (mut coordinator, command_tx, _admission_tx, mut admissions) = fixture();
    insert_pending(&mut coordinator, "child", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    assert!(admissions.try_recv().is_err());

    finish_child(&mut coordinator, "child");
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(response).await
    );
    assert!(admissions.try_recv().is_err());
}

#[tokio::test]
async fn send_to_owned_completed_is_immediate() {
    let (mut coordinator, command_tx, admission_tx, _admissions) = fixture();
    insert_child(&mut coordinator, admission_tx, "child", "parent");
    finish_child(&mut coordinator, "child");
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(begin_send(&mut coordinator, &command_tx, "child", "parent")).await
    );
}

#[tokio::test]
async fn parent_session_cancel_unblocks_parked_send() {
    let (mut coordinator, command_tx, _admission_tx, mut admissions) = fixture();
    let spawn_result = insert_queued(&mut coordinator, "child", "parent");
    let mut response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    assert!(response.try_recv().is_err());

    assert!(matches!(
        coordinator.cancel_parent_session(Some("parent")),
        crate::implementations::grok_build::task::types::SubagentCancelOutcome::Cancelled
    ));
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(response).await
    );
    let terminal = response_outcome_result(spawn_result).await;
    assert!(
        terminal.cancelled && !terminal.success,
        "session cancel must resolve a terminal result: {terminal:?}"
    );
    assert!(admissions.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn send_to_owned_pending_hits_spawn_ready_backstop() {
    let (mut coordinator, command_tx, _admission_tx, mut admissions) = fixture();
    insert_pending(&mut coordinator, "child", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    tokio::time::advance(ACTIVE_MESSAGE_SPAWN_READY_TIMEOUT).await;
    coordinator.expire_spawn_ready_messages(tokio::time::Instant::now());
    assert_eq!(
        ActiveAgentMessageOutcome::NotAcceptedBeforeDeadline,
        response_outcome(response).await
    );
    assert!(admissions.try_recv().is_err());
}

fn insert_queued(
    coordinator: &mut TestCoordinator,
    id: &str,
    parent: &str,
) -> oneshot::Receiver<SubagentResult> {
    let mut request =
        crate::implementations::grok_build::task::coordinator::tests::request(id, true);
    request.parent_session_id = parent.to_owned();
    request.surface_completion = false;
    let (result_tx, result_rx) = oneshot::channel();
    coordinator
        .queued
        .push_back(super::super::queue::QueuedSpawn {
            request: Box::new(request),
            queued_at: tokio::time::Instant::now(),
            caller: super::super::queue::QueuedCaller::Awaiting {
                result_tx,
                deadline: None,
            },
        });
    result_rx
}

#[tokio::test]
async fn send_to_owned_queued_child_parks_until_started() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    let mut spawn_result = insert_queued(&mut coordinator, "child", "parent");
    let mut response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    assert!(response.try_recv().is_err());
    assert!(admissions.try_recv().is_err());

    // Dequeue into pending, then promote — the parked send must admit.
    let queued = coordinator.queued.pop_front().expect("queued child");
    coordinator.start_child(
        *queued.request,
        queued.caller.into_spawn_reply(),
        None,
        super::super::queue::StartOrigin::Dequeued {
            queued_for: std::time::Duration::ZERO,
            deadline: None,
        },
    );
    promote_pending(&mut coordinator, admission_tx, "child");
    let call = recv_with_timeout(&mut admissions).await;
    let message_id = call.message_id.clone();
    release_admission(&mut coordinator, call, ActiveMessageAdmission::Admitted).await;
    assert_eq!(
        ActiveAgentMessageOutcome::Accepted { message_id },
        response_outcome(response).await
    );
    assert!(spawn_result.try_recv().is_err(), "child still running");
}

#[tokio::test]
async fn parked_send_is_saturated_when_admit_cannot_reacquire() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture_with_capacity(1);
    insert_pending(&mut coordinator, "spawning", "parent");
    let mut parked = begin_send(&mut coordinator, &command_tx, "spawning", "parent");
    assert!(parked.try_recv().is_err());

    insert_child(&mut coordinator, admission_tx, "holder", "parent");
    let _held = begin_send(&mut coordinator, &command_tx, "holder", "parent");
    let _call = recv_with_timeout(&mut admissions).await;

    promote_pending(&mut coordinator, mpsc::unbounded_channel().0, "spawning");
    assert_eq!(
        ActiveAgentMessageOutcome::Saturated { max_in_flight: 1 },
        response_outcome(parked).await
    );
}

#[tokio::test]
async fn send_to_cancelled_or_workflow_spawning_child_fails_fast() {
    let (mut coordinator, command_tx, _admission_tx, mut admissions) = fixture();
    insert_pending(&mut coordinator, "cancelled", "parent");
    coordinator
        .pending
        .get("cancelled")
        .expect("pending")
        .cancellation
        .cancel();
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(begin_send(
            &mut coordinator,
            &command_tx,
            "cancelled",
            "parent",
        ))
        .await
    );

    insert_pending(&mut coordinator, "wf", "parent");
    coordinator
        .pending
        .get_mut("wf")
        .expect("pending")
        .request
        .owner = crate::implementations::grok_build::task::types::SubagentOwner::workflow("run-1");
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(begin_send(&mut coordinator, &command_tx, "wf", "parent")).await
    );
    assert!(admissions.try_recv().is_err());
}

#[tokio::test]
async fn cancel_parent_prompt_rejects_parked_send() {
    let (mut coordinator, command_tx, _admission_tx, mut admissions) = fixture();
    let spawn_result = insert_queued(&mut coordinator, "child", "parent");
    let response = begin_send(&mut coordinator, &command_tx, "child", "parent");
    coordinator.cancel_parent_prompt("prompt", Some("parent"));
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(response).await
    );
    let terminal = response_outcome_result(spawn_result).await;
    assert!(
        terminal.cancelled && !terminal.success,
        "queued cancel must resolve a terminal result: {terminal:?}"
    );
    assert!(admissions.try_recv().is_err());
}

#[tokio::test]
async fn cancel_workflow_run_rejects_parked_send() {
    let (mut coordinator, _command_tx, _admission_tx, mut admissions) = fixture();
    insert_pending(&mut coordinator, "child", "parent");
    coordinator
        .pending
        .get_mut("child")
        .expect("pending")
        .request
        .owner = crate::implementations::grok_build::task::types::SubagentOwner::workflow("run-1");
    let (tx, rx) = oneshot::channel();
    coordinator
        .spawn_ready
        .push(super::ParkedSpawnReadyMessage {
            subagent_id: "child".to_owned(),
            parent_session_id: "parent".to_owned(),
            request: ActiveAgentMessageRequest::try_new("child", "hello").unwrap(),
            respond_to: Some(tx),
            deadline: tokio::time::Instant::now() + ACTIVE_MESSAGE_SPAWN_READY_TIMEOUT,
        });
    coordinator.cancel_workflow_children("run-1", Some("parent"));
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(rx).await
    );
    assert!(admissions.try_recv().is_err());
}
