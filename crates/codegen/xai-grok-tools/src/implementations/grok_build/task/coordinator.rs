//! Single-writer subagent coordinator actor.
//!
//! The actor owns the command receiver, pending/active/completed state,
//! concrete blocking waiters, foreground deadlines, cancellation, and the
//! terminal delivery disposition. All hosts drive it through `ChannelBackend`;
//! only their `ChildRunner` implementations differ.
//!
//! There is intentionally no shared mutable state in this module. A runner's
//! associated futures may be `Send` or non-`Send`; the resulting actor future
//! inherits that property naturally on stable Rust.

mod query;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use futures::FutureExt;
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::{mpsc, oneshot};

use super::coordinator_state::{
    ActiveChild, BlockingWaiter, BufferedCompletion, ChildRecord, CompletedChild, InternalEvent,
    ListRequest, MAX_COMPLETED_ENTRIES, PendingChild, ProgressFuture, ProgressTarget, ReplyFuture,
    TaggedFuture, active_summary, background_at_deadline, background_if_caller_gone,
    completed_snapshot, completion_summary, sleep_until, workflow_outstanding,
};
use super::types::{
    SpawnedSubagentRef, SubagentCancelOutcome, SubagentCancelTarget, SubagentDescribeOutcome,
    SubagentEvent, SubagentOutstandingReply, SubagentRegistryCounts, SubagentRequest,
    SubagentResult, SubagentResumeLookup, SubagentResumeSource, SubagentValidateTypeOutcome,
};

pub use super::coordinator_state::{
    ChildCompletion, ChildControl, ChildReporter, ChildRunOutput, ChildRunRequest, ChildRunner,
    CompletionDisposition, CoordinatorConfig, LocalBoxFuture, SendBoxFuture, StartedChild,
    SubagentProgress,
};

/// Channel-owned subagent lifecycle actor.
pub struct SubagentCoordinator<R: ChildRunner> {
    commands: mpsc::UnboundedReceiver<SubagentEvent>,
    internal_tx: mpsc::UnboundedSender<InternalEvent<R::Control>>,
    internal_rx: mpsc::UnboundedReceiver<InternalEvent<R::Control>>,
    runner: R,
    config: CoordinatorConfig,
    pending: HashMap<String, PendingChild>,
    active: HashMap<String, ActiveChild<R::Control>>,
    completed: HashMap<String, CompletedChild>,
    completed_order: VecDeque<String>,
    waiters: HashMap<String, Vec<BlockingWaiter>>,
    workflow_cancel_waiters: HashMap<String, Vec<oneshot::Sender<SubagentCancelOutcome>>>,
    usage_not_applied_prompts: HashSet<PromptScope>,
    pending_completions: Vec<BufferedCompletion>,
    runs: FuturesUnordered<
        TaggedFuture<futures::future::CatchUnwind<std::panic::AssertUnwindSafe<R::RunFuture>>>,
    >,
    validations: FuturesUnordered<ReplyFuture<R::ValidateFuture, SubagentValidateTypeOutcome>>,
    descriptions: FuturesUnordered<ReplyFuture<R::DescribeFuture, SubagentDescribeOutcome>>,
    progress: FuturesUnordered<ProgressFuture<<R::Control as ChildControl>::ProgressFuture>>,
    list_requests: HashMap<u64, ListRequest>,
    next_list_request_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PromptScope {
    parent_session_id: String,
    prompt_id: String,
}

impl PromptScope {
    fn new(parent_session_id: String, prompt_id: String) -> Self {
        Self {
            parent_session_id,
            prompt_id,
        }
    }
}

impl<R: ChildRunner> SubagentCoordinator<R> {
    pub fn new(
        commands: mpsc::UnboundedReceiver<SubagentEvent>,
        runner: R,
        config: CoordinatorConfig,
    ) -> Self {
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        Self {
            commands,
            internal_tx,
            internal_rx,
            runner,
            config,
            pending: HashMap::new(),
            active: HashMap::new(),
            completed: HashMap::new(),
            completed_order: VecDeque::new(),
            waiters: HashMap::new(),
            workflow_cancel_waiters: HashMap::new(),
            usage_not_applied_prompts: HashSet::new(),
            pending_completions: Vec::new(),
            runs: FuturesUnordered::new(),
            validations: FuturesUnordered::new(),
            descriptions: FuturesUnordered::new(),
            progress: FuturesUnordered::new(),
            list_requests: HashMap::new(),
            next_list_request_id: 0,
        }
    }

    pub async fn run(mut self) {
        let mut commands_open = true;
        loop {
            if !commands_open
                && self.runs.is_empty()
                && self.validations.is_empty()
                && self.descriptions.is_empty()
                && self.progress.is_empty()
            {
                break;
            }

            let deadline = self.next_deadline();
            tokio::select! {
                biased;
                Some(event) = self.internal_rx.recv() => self.handle_internal(event),
                Some((id, output)) = self.runs.next(), if !self.runs.is_empty() => {
                    match output {
                        Ok(output) => self.finish_child(&id, output),
                        Err(_) => self.finish_panicked_child(&id),
                    }
                }
                Some((respond_to, outcome)) = self.validations.next(), if !self.validations.is_empty() => {
                    let _ = respond_to.send(outcome);
                }
                Some((respond_to, outcome)) = self.descriptions.next(), if !self.descriptions.is_empty() => {
                    let _ = respond_to.send(outcome);
                }
                Some((seed, target, progress)) = self.progress.next(), if !self.progress.is_empty() => {
                    self.finish_progress(seed, target, progress);
                }
                command = self.commands.recv(), if commands_open => {
                    match command {
                        Some(command) => {
                            self.reap_abandoned_callers();
                            self.handle_command(command);
                        }
                        None => commands_open = false,
                    }
                }
                _ = sleep_until(deadline), if deadline.is_some() => self.process_deadlines(),
            }
            while self.completed.len() > MAX_COMPLETED_ENTRIES {
                let Some(id) = self.completed_order.pop_front() else {
                    break;
                };
                self.completed.remove(&id);
            }
        }

        self.cancel_all_children();
    }

    fn handle_command(&mut self, command: SubagentEvent) {
        match command {
            SubagentEvent::Spawn(command) => {
                let mut request = *command.request;
                if let Some((root_parent, loop_task_id)) = self
                    .active
                    .values()
                    .find(|child| child.child_session_id == request.parent_session_id)
                    .map(|child| {
                        (
                            child.request.parent_session_id.clone(),
                            child.request.runtime_overrides.loop_task_id.clone(),
                        )
                    })
                {
                    request.parent_session_id = root_parent;
                    request.surface_completion = false;
                    if request.runtime_overrides.loop_task_id.is_none() {
                        request.runtime_overrides.loop_task_id = loop_task_id;
                    }
                }
                let id = request.id.clone();
                if self.pending.contains_key(&id)
                    || self.active.contains_key(&id)
                    || self.completed.contains_key(&id)
                {
                    let _ = command.result_tx.send(SubagentResult {
                        success: false,
                        error: Some(format!("Subagent id '{id}' already exists")),
                        subagent_id: id.clone(),
                        child_session_id: id,
                        ..Default::default()
                    });
                    return;
                }
                let cancellation = request.cancel_token.clone();
                let handle_only = request.run_in_background;
                let foreground_deadline = (!request.run_in_background
                    && !request.await_to_completion)
                    .then(|| tokio::time::Instant::now() + self.config.foreground_budget);
                self.pending.insert(
                    id.clone(),
                    PendingChild {
                        request: request.clone(),
                        started_at: std::time::Instant::now(),
                        cancellation: cancellation.clone(),
                        spawn_reply: Some(command.result_tx),
                        foreground_deadline,
                        handle_only,
                        explicitly_killed: false,
                    },
                );
                self.running_count_changed();
                let reporter = ChildReporter {
                    subagent_id: id.clone(),
                    tx: self.internal_tx.clone(),
                };
                self.runs.push(TaggedFuture {
                    subagent_id: id,
                    future: Box::pin(
                        std::panic::AssertUnwindSafe(self.runner.run(ChildRunRequest {
                            request,
                            cancellation,
                            reporter,
                        }))
                        .catch_unwind(),
                    ),
                });
            }
            SubagentEvent::Query(query) => {
                self.handle_query(
                    query.subagent_id,
                    query.parent_session_id,
                    query.block,
                    query.timeout_ms,
                    query.respond_to,
                );
            }
            SubagentEvent::Cancel(request) => match request.target {
                SubagentCancelTarget::SubagentId(id) => {
                    let outcome = self.cancel_one(&id, request.parent_session_id.as_deref(), true);
                    let _ = request.respond_to.send(outcome);
                }
                SubagentCancelTarget::ParentPromptId(prompt_id) => {
                    self.cancel_parent_prompt(&prompt_id, request.parent_session_id.as_deref());
                    let _ = request.respond_to.send(SubagentCancelOutcome::Cancelled);
                }
                SubagentCancelTarget::WorkflowRunId(run_id) => {
                    self.cancel_workflow_children(&run_id, request.parent_session_id.as_deref());
                    if workflow_outstanding(&self.pending, &self.active, &run_id) == 0 {
                        let _ = request.respond_to.send(SubagentCancelOutcome::Cancelled);
                    } else {
                        self.workflow_cancel_waiters
                            .entry(run_id)
                            .or_default()
                            .push(request.respond_to);
                    }
                }
            },
            SubagentEvent::ListActive(request) => {
                let summaries = self
                    .active
                    .values()
                    .filter(|child| {
                        child.request.parent_session_id == request.parent_session_id
                            && !child.request.owner.is_workflow()
                    })
                    .map(active_summary)
                    .collect();
                let _ = request.respond_to.send(summaries);
            }
            SubagentEvent::ListRunning(request) => {
                self.handle_list_running(request.parent_session_id, request.respond_to);
            }
            SubagentEvent::Completions(request) => {
                let (owned, foreign): (Vec<_>, Vec<_>) =
                    std::mem::take(&mut self.pending_completions)
                        .into_iter()
                        .partition(|completion| {
                            request
                                .parent_session_id
                                .as_ref()
                                .is_none_or(|id| completion.parent_session_id == *id)
                        });
                self.pending_completions = foreign;
                let completions = owned
                    .into_iter()
                    .map(|completion| completion.summary)
                    .filter(|summary| !request.suppress_ids.contains(&summary.subagent_id))
                    .collect();
                let _ = request.respond_to.send(completions);
            }
            SubagentEvent::DiscardSessionCompletions { parent_session_id } => {
                self.pending_completions
                    .retain(|completion| completion.parent_session_id != parent_session_id);
            }
            SubagentEvent::Outstanding(request) => {
                // Reap again here so turn-freeze / Outstanding polls see
                // ParentGone even if no other command woke the actor first.
                self.reap_abandoned_callers();
                let mut live_ids: Vec<_> = self
                    .pending
                    .values()
                    .filter(|child| {
                        child.request.parent_session_id == request.parent_session_id
                            && child.request.parent_prompt_id.as_deref() == Some(&request.prompt_id)
                            && !child.request.owner.is_workflow()
                            && !child.handle_only
                    })
                    .map(|child| child.request.id.clone())
                    .chain(
                        self.active
                            .values()
                            .filter(|child| {
                                child.request.parent_session_id == request.parent_session_id
                                    && child.request.parent_prompt_id.as_deref()
                                        == Some(&request.prompt_id)
                                    && !child.request.owner.is_workflow()
                                    // Definition-declared background children are
                                    // background for accounting even while the
                                    // spawning tool block-awaits them.
                                    && !child.handle_only
                                    && !child.definition_background
                            })
                            .map(|child| child.request.id.clone()),
                    )
                    .collect();
                live_ids.sort();
                let background_live = self.pending.values().any(|child| {
                    child.request.parent_session_id == request.parent_session_id
                        && child.request.parent_prompt_id.as_deref() == Some(&request.prompt_id)
                        && !child.request.owner.is_workflow()
                        && child.handle_only
                }) || self.active.values().any(|child| {
                    child.request.parent_session_id == request.parent_session_id
                        && child.request.parent_prompt_id.as_deref() == Some(&request.prompt_id)
                        && !child.request.owner.is_workflow()
                        && (child.handle_only || child.definition_background)
                });
                let scope =
                    PromptScope::new(request.parent_session_id.clone(), request.prompt_id.clone());
                let _ = request.respond_to.send(SubagentOutstandingReply {
                    live_ids,
                    background_live,
                    subagent_usage_not_applied: self.usage_not_applied_prompts.contains(&scope),
                });
            }
            SubagentEvent::ClearUsageNotApplied(request) => {
                self.usage_not_applied_prompts.remove(&PromptScope::new(
                    request.parent_session_id,
                    request.prompt_id,
                ));
            }
            SubagentEvent::MarkUsageNotApplied(request) => {
                self.usage_not_applied_prompts.insert(PromptScope::new(
                    request.parent_session_id,
                    request.prompt_id,
                ));
                let _ = request.respond_to.send(());
            }
            SubagentEvent::RegistryCounts(request) => {
                let _ = request.respond_to.send(SubagentRegistryCounts {
                    pending: self.pending.len(),
                    active: self.active.len(),
                    completed: self.completed.len(),
                });
            }
            SubagentEvent::Inspect(request) => {
                self.handle_inspect(
                    request.subagent_id,
                    request.parent_session_id,
                    request.respond_to,
                );
            }
            SubagentEvent::SpawnedRefs(request) => {
                let mut refs: Vec<_> = self
                    .active
                    .values()
                    .filter(|child| {
                        child.request.parent_session_id == request.parent_session_id
                            && child.request.parent_prompt_id.as_deref() == Some(&request.prompt_id)
                    })
                    .map(|child| SpawnedSubagentRef {
                        subagent_id: child.request.id.clone(),
                        child_session_id: child.child_session_id.clone(),
                        subagent_type: child.request.subagent_type.clone(),
                        description: child.request.description.clone(),
                        persona: child.persona.clone(),
                        resumed_from: child.resumed_from.clone(),
                    })
                    .chain(
                        self.completed
                            .values()
                            .filter(|child| {
                                child.request.parent_session_id == request.parent_session_id
                                    && child.request.parent_prompt_id.as_deref()
                                        == Some(&request.prompt_id)
                            })
                            .map(|child| SpawnedSubagentRef {
                                subagent_id: child.request.id.clone(),
                                child_session_id: child.child_session_id.clone(),
                                subagent_type: child.request.subagent_type.clone(),
                                description: child.request.description.clone(),
                                persona: child.persona.clone(),
                                resumed_from: child.resumed_from.clone(),
                            }),
                    )
                    .collect();
                refs.sort_by(|a, b| a.subagent_id.cmp(&b.subagent_id));
                let _ = request.respond_to.send(refs);
            }
            SubagentEvent::ValidateType(request) => {
                self.validations.push(ReplyFuture {
                    future: Box::pin(
                        self.runner
                            .validate_type(request.subagent_type, request.parent_session_id),
                    ),
                    respond_to: Some(request.respond_to),
                });
            }
            SubagentEvent::DescribeType(request) => {
                self.descriptions.push(ReplyFuture {
                    future: Box::pin(self.runner.describe_type(
                        request.subagent_type,
                        request.harness_agent_type,
                        request.parent_session_id,
                    )),
                    respond_to: Some(request.respond_to),
                });
            }
            SubagentEvent::LoopUnitActive(request) => {
                let is_active = self.pending.values().any(|child| {
                    child.request.runtime_overrides.loop_task_id.as_deref()
                        == Some(&request.task_id)
                }) || self.active.values().any(|child| {
                    child.request.runtime_overrides.loop_task_id.as_deref()
                        == Some(&request.task_id)
                });
                let _ = request.respond_to.send(is_active);
            }
        }
    }

    fn handle_internal(&mut self, event: InternalEvent<R::Control>) {
        match event {
            InternalEvent::Started {
                subagent_id,
                child,
                respond_to,
            } => {
                let Some(pending) = self.pending.remove(&subagent_id) else {
                    let _ = respond_to.send(false);
                    return;
                };
                if pending.cancellation.is_cancelled() {
                    self.pending.insert(subagent_id, pending);
                    let _ = respond_to.send(false);
                    return;
                }
                self.active.insert(
                    subagent_id,
                    ActiveChild {
                        request: pending.request,
                        started_at: pending.started_at,
                        cancellation: pending.cancellation,
                        spawn_reply: pending.spawn_reply,
                        foreground_deadline: pending.foreground_deadline,
                        handle_only: pending.handle_only,
                        definition_background: child.definition_background,
                        explicitly_killed: pending.explicitly_killed,
                        child_session_id: child.child_session_id,
                        persona: child.persona,
                        resumed_from: child.resumed_from,
                        child_cwd: child.child_cwd,
                        worktree_path: child.worktree_path,
                        effective_model_id: child.effective_model_id,
                        control: child.control,
                    },
                );
                let _ = respond_to.send(true);
            }
            InternalEvent::ResumeSource {
                source_id,
                parent_session_id,
                respond_to,
            } => {
                let source_is_active =
                    self.pending
                        .get(&source_id)
                        .is_some_and(|child| child.request.parent_session_id == parent_session_id)
                        || self.active.get(&source_id).is_some_and(|child| {
                            child.request.parent_session_id == parent_session_id
                        });
                let lookup = if source_is_active {
                    SubagentResumeLookup::Active
                } else if let Some(child) = self.completed.get(&source_id)
                    && child.request.parent_session_id == parent_session_id
                {
                    SubagentResumeLookup::Completed(SubagentResumeSource {
                        subagent_id: child.request.id.clone(),
                        child_session_id: child.child_session_id.clone(),
                        child_cwd: child.child_cwd.clone(),
                        worktree_path: child.worktree_path.clone(),
                        snapshot_ref: child.snapshot_ref.clone(),
                        subagent_type: child.request.subagent_type.clone(),
                        persona: child.persona.clone(),
                        model_id: Some(child.effective_model_id.clone()),
                    })
                } else {
                    SubagentResumeLookup::Missing
                };
                let _ = respond_to.send(lookup);
            }
        }
    }

    fn finish_child(&mut self, id: &str, output: ChildRunOutput<R::CompletionData>) {
        let record = if let Some(child) = self.active.remove(id) {
            ChildRecord::Active(child)
        } else if let Some(child) = self.pending.remove(id) {
            ChildRecord::Pending(child)
        } else {
            return;
        };

        let request = record.request().clone();
        let explicitly_killed = record.explicitly_killed();
        let (
            started_at,
            child_session_id,
            persona,
            resumed_from,
            child_cwd,
            worktree_path,
            effective_model_id,
            mut spawn_reply,
            mut handle_only,
        ) = match record {
            ChildRecord::Pending(child) => (
                child.started_at,
                output.result.child_session_id.clone(),
                child.request.runtime_overrides.persona.clone(),
                child.request.resume_from.clone(),
                child.request.cwd.clone().unwrap_or_default(),
                output.result.worktree_path.clone(),
                String::new(),
                child.spawn_reply,
                child.handle_only,
            ),
            ChildRecord::Active(child) => (
                child.started_at,
                child.child_session_id,
                child.persona,
                child.resumed_from,
                child.child_cwd,
                child.worktree_path,
                child.effective_model_id,
                child.spawn_reply,
                child.handle_only,
            ),
        };

        let persisted_output_ref = self.runner.persisted_output_ref(&output.completion_data);
        let mut completed = CompletedChild {
            request: request.clone(),
            started_at,
            child_session_id,
            persona,
            resumed_from,
            child_cwd,
            worktree_path,
            snapshot_ref: output.snapshot_ref,
            persisted_output_ref,
            effective_model_id,
            result: output.result.clone(),
        };
        let snapshot = completed_snapshot(&completed, None);

        let mut waiter_delivered = false;
        for waiter in self.waiters.remove(id).unwrap_or_default() {
            waiter_delivered |= waiter.respond_to.send(Some(snapshot.clone())).is_ok();
        }

        let mut foreground_delivered = false;
        if let Some(respond_to) = spawn_reply.take() {
            let sent = respond_to.send(output.result.clone()).is_ok();
            if !handle_only {
                foreground_delivered = sent;
                handle_only = !sent;
            }
        } else if !handle_only {
            handle_only = true;
        }

        if self.config.buffer_completions
            && request.surface_completion
            && !request.owner.is_workflow()
        {
            let mut summary = completion_summary(&request, &output.result);
            if let Some(cap) = self.config.buffered_completion_output_cap {
                summary.output = super::cap_completion_output(&summary.output, cap);
            }
            self.pending_completions.push(BufferedCompletion {
                parent_session_id: request.parent_session_id.clone(),
                summary,
            });
            // Bound the buffer (drop oldest): sessions unloaded without a
            // DiscardSessionCompletions cannot grow it unboundedly.
            const MAX_PENDING_COMPLETIONS: usize = 256;
            if self.pending_completions.len() > MAX_PENDING_COMPLETIONS {
                let excess = self.pending_completions.len() - MAX_PENDING_COMPLETIONS;
                self.pending_completions.drain(..excess);
            }
        }
        if completed.persisted_output_ref.is_some() {
            completed.result.output = Arc::from("");
        }

        let should_surface = request.surface_completion
            && handle_only
            && !output.result.cancelled
            && !waiter_delivered
            && !explicitly_killed;
        let disposition = CompletionDisposition {
            foreground_delivered,
            backgrounded: handle_only,
            waiter_delivered,
            explicitly_killed,
            should_surface,
        };
        self.completed.insert(id.to_owned(), completed);
        self.completed_order.push_back(id.to_owned());
        self.running_count_changed();
        let workflow_run_id = request.owner.workflow_run_id().map(str::to_owned);
        self.runner.on_completed(ChildCompletion {
            request,
            result: output.result,
            completion_data: output.completion_data,
            disposition,
        });
        if let Some(run_id) = workflow_run_id {
            self.resolve_workflow_cancel_waiters(&run_id);
        }
    }

    fn finish_panicked_child(&mut self, id: &str) {
        let request = self
            .active
            .get(id)
            .map(|child| child.request.clone())
            .or_else(|| self.pending.get(id).map(|child| child.request.clone()));
        let Some(request) = request else {
            return;
        };
        tracing::error!(subagent_id = id, "subagent child runner panicked");
        self.finish_child(
            id,
            ChildRunOutput {
                result: SubagentResult {
                    success: false,
                    error: Some("Subagent runtime panicked".to_owned()),
                    subagent_id: request.id.clone(),
                    child_session_id: request.id,
                    ..Default::default()
                },
                completion_data: R::CompletionData::default(),
                snapshot_ref: None,
            },
        );
    }

    fn cancel_one(
        &mut self,
        id: &str,
        parent_session_id: Option<&str>,
        explicit: bool,
    ) -> SubagentCancelOutcome {
        if let Some(child) = self.active.get_mut(id)
            && belongs_to_session(&child.request, parent_session_id)
        {
            child.explicitly_killed |= explicit;
            child.cancellation.cancel();
            child.control.cancel();
            return SubagentCancelOutcome::Cancelled;
        }
        if let Some(child) = self.pending.get_mut(id)
            && belongs_to_session(&child.request, parent_session_id)
        {
            child.explicitly_killed |= explicit;
            child.cancellation.cancel();
            return SubagentCancelOutcome::Cancelled;
        }
        if let Some(child) = self.completed.get(id)
            && belongs_to_session(&child.request, parent_session_id)
        {
            return SubagentCancelOutcome::AlreadyFinished {
                status: child.result.status().to_owned(),
            };
        }
        SubagentCancelOutcome::NotFound
    }

    fn cancel_parent_prompt(&mut self, parent_prompt_id: &str, parent_session_id: Option<&str>) {
        for child in self.active.values() {
            if child.request.parent_prompt_id.as_deref() == Some(parent_prompt_id)
                && belongs_to_session(&child.request, parent_session_id)
            {
                child.cancellation.cancel();
                child.control.cancel();
            }
        }
        for child in self.pending.values() {
            if child.request.parent_prompt_id.as_deref() == Some(parent_prompt_id)
                && belongs_to_session(&child.request, parent_session_id)
            {
                child.cancellation.cancel();
            }
        }
    }

    fn cancel_workflow_children(&mut self, run_id: &str, parent_session_id: Option<&str>) {
        for child in self.active.values() {
            if child.request.owner.workflow_run_id() == Some(run_id)
                && belongs_to_session(&child.request, parent_session_id)
            {
                child.cancellation.cancel();
                child.control.cancel();
            }
        }
        for child in self.pending.values() {
            if child.request.owner.workflow_run_id() == Some(run_id)
                && belongs_to_session(&child.request, parent_session_id)
            {
                child.cancellation.cancel();
            }
        }
    }

    fn resolve_workflow_cancel_waiters(&mut self, run_id: &str) {
        if workflow_outstanding(&self.pending, &self.active, run_id) != 0 {
            return;
        }
        for respond_to in self
            .workflow_cancel_waiters
            .remove(run_id)
            .unwrap_or_default()
        {
            let _ = respond_to.send(SubagentCancelOutcome::Cancelled);
        }
    }

    fn next_deadline(&self) -> Option<tokio::time::Instant> {
        self.pending
            .values()
            .filter_map(|child| child.foreground_deadline)
            .chain(
                self.active
                    .values()
                    .filter_map(|child| child.foreground_deadline),
            )
            .chain(
                self.waiters
                    .values()
                    .flatten()
                    .map(|waiter| waiter.deadline),
            )
            .min()
    }

    fn reap_abandoned_callers(&mut self) {
        for child in self.pending.values_mut() {
            background_if_caller_gone(child);
        }
        for child in self.active.values_mut() {
            background_if_caller_gone(child);
        }
    }

    fn process_deadlines(&mut self) {
        self.reap_abandoned_callers();
        let now = tokio::time::Instant::now();
        for child in self.pending.values_mut() {
            background_at_deadline(child, now, self.config.foreground_budget);
        }
        for child in self.active.values_mut() {
            background_at_deadline(child, now, self.config.foreground_budget);
        }

        let ids: Vec<_> = self.waiters.keys().cloned().collect();
        for id in ids {
            let waiters = self.waiters.remove(&id).unwrap_or_default();
            let (due, live): (Vec<_>, Vec<_>) = waiters
                .into_iter()
                .partition(|waiter| waiter.deadline <= now);
            if !live.is_empty() {
                self.waiters.insert(id.clone(), live);
            }
            for waiter in due {
                if waiter.respond_to.is_closed() {
                    continue;
                }
                if self.active.contains_key(&id) {
                    self.queue_active_progress(&id, ProgressTarget::Query(waiter.respond_to));
                } else {
                    let _ = waiter.respond_to.send(self.ready_snapshot(&id));
                }
            }
        }
    }

    fn running_count_changed(&self) {
        self.runner
            .running_count_changed(self.pending.len() + self.active.len());
    }

    fn cancel_all_children(&self) {
        for child in self.active.values() {
            child.cancellation.cancel();
            child.control.cancel();
        }
        for child in self.pending.values() {
            child.cancellation.cancel();
        }
    }
}

fn belongs_to_session(request: &SubagentRequest, parent_session_id: Option<&str>) -> bool {
    parent_session_id.is_none_or(|id| request.parent_session_id == id)
}

impl<R: ChildRunner> Drop for SubagentCoordinator<R> {
    fn drop(&mut self) {
        self.cancel_all_children();
    }
}

#[cfg(test)]
#[path = "coordinator_tests.rs"]
mod tests;
