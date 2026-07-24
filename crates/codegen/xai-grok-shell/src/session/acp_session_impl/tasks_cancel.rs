//! Prompt-task plumbing for `SessionActor` (`AgentTask`, `TaskSlot`,
//! `run_task`, turn guards) and the cancel paths.

use super::*;

pub(super) struct TurnSubagentScopeGuard {
    current_prompt_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    prompt_id: String,
}

impl TurnSubagentScopeGuard {
    pub(super) fn new(
        current_prompt_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
        prompt_id: String,
    ) -> Self {
        Self {
            current_prompt_id,
            prompt_id,
        }
    }
}

impl Drop for TurnSubagentScopeGuard {
    fn drop(&mut self) {
        let mut current_prompt_id = self
            .current_prompt_id
            .lock()
            .expect("current_prompt_id mutex poisoned");
        if current_prompt_id.as_deref() == Some(self.prompt_id.as_str()) {
            *current_prompt_id = None;
        }
    }
}

/// RAII guard that stores `false` into an `is_turn_active` flag on drop.
/// Guarantees the flag is cleared on all exit paths (early returns, errors, panics).
pub(super) struct TurnActiveGuard(Option<Arc<std::sync::atomic::AtomicBool>>);

impl TurnActiveGuard {
    pub(super) fn activate(flag: Option<&Arc<std::sync::atomic::AtomicBool>>) -> Self {
        if let Some(f) = flag {
            f.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Self(flag.cloned())
    }
}

impl Drop for TurnActiveGuard {
    fn drop(&mut self) {
        if let Some(flag) = &self.0 {
            flag.store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

pub(crate) struct AgentTask {
    pub(crate) prompt_id: String,
    pub(crate) handle: tokio::task::AbortHandle,
}

impl AgentTask {
    pub(super) fn new_prompt(
        session: Arc<SessionActor>,
        prompt_id: String,
        input: Vec<ContentBlock>,
        prompt_mode: PromptMode,
        trace_gcs_config: Option<crate::session::repo_changes::TraceExportConfig>,
        artifact_tracker: Option<crate::upload::manifest::ArtifactTracker>,
        client_identifier: Option<String>,
        screen_mode: Option<String>,
        verbatim: bool,
        json_schema: Option<serde_json::Value>,
        completion_tx: mpsc::UnboundedSender<(String, PromptTurnResult)>,
        persist_ack: Option<oneshot::Sender<()>>,
        parsed_prompt_tx: Option<oneshot::Sender<ParsedPromptInfo>>,
    ) -> Self {
        let pid = prompt_id.clone();
        Self {
            prompt_id,
            handle: tokio::task::spawn_local(async move {
                run_task(
                    session.clone(),
                    input,
                    prompt_mode,
                    trace_gcs_config,
                    artifact_tracker,
                    client_identifier,
                    screen_mode,
                    verbatim,
                    json_schema,
                    pid,
                    completion_tx,
                    persist_ack,
                    parsed_prompt_tx,
                )
                .await
            })
            .abort_handle(),
        }
    }

    fn abort(&self) {
        if !self.handle.is_finished() {
            self.handle.abort();
        }
    }
}

/// Holds at most one spawned task; arming a new one aborts the previous.
/// `Cell` interior mutability because `SessionActor` is `!Send` (single-threaded
/// LocalSet). Backs both the deferred user-message prefix (`take`s the handle to
/// await its result) and the idle-notification debounce (`cancel`s it).
pub(crate) struct TaskSlot<T> {
    handle: std::cell::Cell<Option<tokio::task::JoinHandle<T>>>,
}

impl<T> TaskSlot<T> {
    pub(crate) fn new() -> Self {
        Self {
            handle: std::cell::Cell::new(None),
        }
    }

    /// Abort any pending task and store the new one.
    pub(crate) fn arm(&self, handle: tokio::task::JoinHandle<T>) {
        if let Some(old) = self.handle.take() {
            old.abort();
        }
        self.handle.set(Some(handle));
    }

    /// Take the pending task handle (e.g. to await its result).
    pub(crate) fn take(&self) -> Option<tokio::task::JoinHandle<T>> {
        self.handle.take()
    }

    /// Abort and drop any pending task (e.g. because a new turn started).
    pub(crate) fn cancel(&self) {
        if let Some(old) = self.handle.take() {
            old.abort();
        }
    }
}

async fn run_task(
    session: Arc<SessionActor>,
    input: Vec<ContentBlock>,
    prompt_mode: PromptMode,
    trace_gcs_config: Option<crate::session::repo_changes::TraceExportConfig>,
    artifact_tracker: Option<crate::upload::manifest::ArtifactTracker>,
    client_identifier: Option<String>,
    screen_mode: Option<String>,
    verbatim: bool,
    json_schema: Option<serde_json::Value>,
    prompt_id: String,
    completion_tx: mpsc::UnboundedSender<(String, PromptTurnResult)>,
    persist_ack: Option<oneshot::Sender<()>>,
    parsed_prompt_tx: Option<oneshot::Sender<ParsedPromptInfo>>,
) {
    let result = session
        .handle_prompt(
            &prompt_id,
            input,
            prompt_mode,
            trace_gcs_config,
            artifact_tracker,
            client_identifier,
            screen_mode,
            verbatim,
            json_schema,
            persist_ack,
            parsed_prompt_tx,
        )
        .await;
    let _ = completion_tx.send((prompt_id, result));
}

impl SessionActor {
    pub(super) fn cancel_running_turn_subagents(&self) {
        let Some(parent_prompt_id) = self
            .current_prompt_id
            .lock()
            .expect("current_prompt_id mutex poisoned")
            .clone()
        else {
            return;
        };

        self.cancel_subagents_for_prompt_id(&parent_prompt_id);
    }

    fn cancel_subagents_for_prompt_id(&self, parent_prompt_id: &str) {
        if let Some(event_tx) = self.tool_context.subagent_event_tx.clone() {
            use xai_grok_tools::implementations::grok_build::task::types::{
                SubagentCancelRequest, SubagentCancelTarget, SubagentEvent,
            };
            let _ = event_tx.send(SubagentEvent::Cancel(SubagentCancelRequest {
                parent_session_id: Some(self.session_id_string()),
                target: SubagentCancelTarget::ParentPromptId(parent_prompt_id.to_string()),
                respond_to: tokio::sync::oneshot::channel().0,
            }));
        }
    }

    /// Cancel the running turn for a send-now prompt: Ctrl+C parity (kills the
    /// turn's foreground command; background tasks, subagents, and the queue
    /// survive). Flushes the replay buffer before teardown so streamed chunks
    /// persist; the caller's `maybe_start_running_task` then promotes the prompt.
    pub(super) async fn cancel_turn_for_send_now(
        &self,
        replay_buffer: &mut crate::agent::update_chunk_merge::ReplayBuffer,
    ) {
        if let Some(notification) = replay_buffer.flush() {
            self.emit_buffered(notification).await;
        }
        self.pending_interjections.clear();
        self.cancel_running_task(false, false, false, Some("send_now".to_string()))
            .await;
        // Re-enable notification drains: unlike Ctrl+C, a send-now means the user is re-engaged.
        if let Some(gate) = &self.tool_context.task_wake_suppressed {
            gate.set(false);
        }
        self.state.lock().await.notifications_suppressed = false;
        xai_grok_telemetry::unified_log::info(
            "shell.task_wake.gate_cleared",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({ "reason": "send_now" })),
        );
    }

    pub(super) async fn cancel_running_task(
        &self,
        cancel_subagents: bool,
        kill_background_tasks: bool,
        rewind_if_pristine: bool,
        trigger: Option<String>,
    ) {
        let suppress_task_wakes = trigger.as_deref() == Some("ctrl_c");
        if suppress_task_wakes {
            if let Some(gate) = &self.tool_context.task_wake_suppressed {
                gate.set(true);
            }
            let mut state = self.state.try_lock().expect("session state is actor-owned");
            state.notifications_suppressed = true;
            xai_grok_telemetry::unified_log::info(
                "shell.task_wake.cancel_barrier",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({
                    "ctrl_c": true,
                    "gate": self
                        .tool_context
                        .task_wake_suppressed
                        .as_ref()
                        .is_some_and(|gate| gate.get()),
                    "state": state.notifications_suppressed,
                })),
            );
            drop(state);
            if let Some(is_turn_active) = &self.tool_context.is_turn_active {
                is_turn_active.store(false, std::sync::atomic::Ordering::Relaxed);
            }
        }

        // Unified-log processing marker (counterpart of `shell.cancel.received`
        // in `MvpAgent::cancel`): records which prompt the cancel lands on so
        // a stuck "Cancelling…" can be attributed to delivery vs. processing.
        // A pin snapshot only — the authoritative cancel identity is captured
        // from `running_task.prompt_id` under the state lock below, because
        // `current_prompt_id` is cleared early (turn scope guard drop /
        // `handle_completion`) while the finished front and its task slot are
        // still queued; keying the durable `TurnCompleted` on the pin alone
        // loses the terminal (and its `cancelTrigger`) in that window.
        let pinned_prompt_id = self
            .current_prompt_id
            .lock()
            .expect("current_prompt_id mutex poisoned")
            .clone();
        {
            xai_grok_telemetry::unified_log::info(
                "shell.cancel.processing",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({
                    "prompt_id": &pinned_prompt_id,
                    "cancel_subagents": cancel_subagents,
                    "kill_background_tasks": kill_background_tasks,
                    "rewind_if_pristine": rewind_if_pristine,
                    "trigger": trigger,
                })),
            );
        }

        if cancel_subagents {
            self.cancel_running_turn_subagents();
        }

        // Don't count send-now/rewound cancels — they'd skew the cancel-rate signal.
        if !rewind_if_pristine && trigger.as_deref() != Some("send_now") {
            self.signals_handle().record_cancellation();
        }

        // Kill all running foreground terminal processes before aborting the task.
        // Each TerminalBackend implementation knows how to kill its own processes.
        // Background tasks are left alive for interactive sessions but killed
        // during subagent teardown (kill_background_tasks = true).
        //
        // Note: a narrow TOCTOU window exists — the running task could spawn a
        // new terminal between this call and the abort() below. In practice this
        // is negligible; abort() drops the future and any child handle it owns.
        if self.startup_hints.is_subagent {
            // Subagent: only kill foreground processes owned by this session,
            // not the parent's or sibling's on the shared backend.
            self.agent
                .borrow()
                .tool_bridge()
                .kill_foreground_commands_by_owner(&self.session_info.id.0)
                .await;
        } else {
            self.agent
                .borrow()
                .tool_bridge()
                .kill_foreground_commands()
                .await;
        }

        if kill_background_tasks {
            if self.startup_hints.is_subagent {
                // Subagent teardown: only kill tasks owned by this session,
                // not the parent's or sibling's tasks on the shared backend.
                self.agent
                    .borrow()
                    .tool_bridge()
                    .kill_all_background_tasks_by_owner(&self.session_info.id.0)
                    .await;
            } else {
                self.agent
                    .borrow()
                    .tool_bridge()
                    .kill_all_background_tasks()
                    .await;
            }
        }

        let total_tokens = self.chat_state_handle.get_total_tokens().await;
        let (running_task, pending_inputs, rewound_input, had_queued_user_prompt) = {
            let mut state = self.state.lock().await;
            debug_assert!(
                pinned_prompt_id.is_none()
                    || state.running_prompt_id().is_none()
                    || state.running_prompt_id() == pinned_prompt_id.as_deref(),
                "current_prompt_id pin disagrees with running_task identity"
            );

            // Drain MonitorEventBuffer into pending_notifications.
            // Closes the race between abort() and TurnActiveGuard drop:
            // is_turn_active may still be true, causing InjectNotification
            // to route Next-priority events to the buffer instead of
            // pending_notifications. Moving them here ensures they survive in
            // the queue; Ctrl+C defers their drain, while other cancels do not.
            self.sweep_monitor_buffer_into_pending(&mut state, "monitor-cancel-drain");

            // When killing all background tasks, also clear their pending
            // notifications — the monitors that produced them are now dead.
            if kill_background_tasks {
                state.clear_pending_notifications();
            }

            let rewound_input = if rewind_if_pristine && state.rewindable {
                if let Some(task) = state.running_task.take() {
                    task.abort();
                }
                if let Some(gate) = &self.tool_context.task_wake_suppressed {
                    gate.set(false);
                }
                state.notifications_suppressed = false;
                xai_grok_telemetry::unified_log::info(
                    "shell.task_wake.gate_cleared",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!({ "reason": "rewind" })),
                );
                state.rewindable = false;
                state.pending_inputs.pop_front()
            } else {
                None
            };
            let running_task = if rewound_input.is_some() {
                None
            } else {
                state.running_task.take()
            };

            // Decide which queued inputs get resolved with `Cancelled` now vs.
            // preserved for the post-cancel drain:
            //
            // * rewind: the front was already popped above; respond to nothing.
            // * hard teardown (`kill_background_tasks`, the subagent-shutdown
            //   path that sends `Shutdown` next): drain the WHOLE queue — there
            //   is no point starting the next prompt and draining resolves every
            //   queued input's `respond_to` cleanly.
            // * normal cancel: remove the running turn; only Ctrl+C also removes
            //   queued task/workflow completion wakes. Preserve real user prompts
            //   and unrelated synthetic entries so `maybe_start_running_task` can
            //   promote the next genuine user turn.
            //   The cancelling client does not pull any prompt back into its
            //   input — the server queue is the single source of truth for what
            //   runs next. Previously every cancel did `std::mem::take`,
            //   discarding the whole queue server-side; because no broadcast
            //   followed, clients kept a stale mirror and the queue only visibly
            //   vanished on the next prompt's (now-empty) broadcast.
            //
            // The in-flight turn is always `pending_inputs.front()`
            // (`maybe_start_running_task` promotes the front WITHOUT popping it;
            // `handle_completion` pops it at turn end) — so index 0 is the slot
            // whose `respond_to` the client is awaiting and whose spinner is
            // showing. We ALWAYS resolve it with `Cancelled`, regardless of
            // whether `running_task` is currently `Some`: in the narrow windows
            // where the front has no live task (e.g. a completion was just
            // dequeued and the next prompt not yet promoted, or a cancel races
            // ahead of `maybe_start_running_task`), gating on `running_task`
            // would drop the front's `respond_to`, hanging the client's
            // `session/prompt` forever (the TUI spinner never returns to idle).
            let pending_inputs = if rewound_input.is_some() {
                VecDeque::new()
            } else if kill_background_tasks {
                std::mem::take(&mut state.pending_inputs)
            } else {
                let mut kept = VecDeque::with_capacity(state.pending_inputs.len());
                let mut cancelled = VecDeque::new();
                for (idx, item) in std::mem::take(&mut state.pending_inputs)
                    .into_iter()
                    .enumerate()
                {
                    let is_running_turn = idx == 0;
                    if is_running_turn {
                        cancelled.push_back(item);
                    } else if suppress_task_wakes
                        && matches!(
                            &item.origin,
                            super::PromptOrigin::TaskCompleted { .. }
                                | super::PromptOrigin::WorkflowCompleted { .. }
                        )
                    {
                        if let Some(fallback) = item.task_wake_fallback {
                            Self::push_task_wake_fallback(&mut state, fallback);
                        }
                        Self::respond_removed_prompt(item.respond_to);
                    } else {
                        kept.push_back(item);
                    }
                }
                state.pending_inputs = kept;
                cancelled
            };
            // Whether a user prompt remains queued behind the just-cancelled
            // turn. It distinguishes the next turn's redirect kind for
            // telemetry: `queued_after_cancel` (a queued prompt is promoted)
            // vs `cancel_then_send` (the user types a fresh prompt). Synthetic
            // inputs (auto-wake / nudges) are not user redirects.
            let had_queued_user_prompt = state
                .pending_inputs
                .iter()
                .any(|i| !i.origin.is_synthetic());
            // NOTE: `current_prompt_id` is deliberately NOT cleared here —
            // cancel usage attribution must snapshot the ledger against the
            // live pin first; it is cleared below, right after the
            // `finalize_usage_from_outcome` / `snapshot_prompt_usage` call.
            (
                running_task,
                pending_inputs,
                rewound_input,
                had_queued_user_prompt,
            )
        };
        // Authoritative cancel identity: the task actually torn down. The pin
        // is only a fallback for windows where a cancel raced ahead of the
        // promote (no task yet) — see the capture comment above.
        let cancelled_prompt_id = running_task
            .as_ref()
            .map(|t| t.prompt_id.clone())
            .or(pinned_prompt_id);

        self.agent
            .borrow()
            .tool_bridge()
            .update_resource(
                xai_grok_tools::implementations::grok_build::task::types::CurrentPromptIdResource(
                    String::new(),
                ),
            )
            .await;
        // Cancellation aborts the in-turn goal loop before its post-loop
        // cleanup runs, so clear the goal-loop flag here too.
        self.set_goal_loop_active_resource(false).await;

        self.events.cancel_active_tool();
        if rewound_input.is_none() {
            // The trigger (esc / ctrl_c / …) rides in the events.jsonl
            // `cancellation_context`; the category stays `MidTurnAbort` so the
            // existing dashboards/dataset keep working.
            let cancellation_context = trigger
                .as_deref()
                .map(|t| serde_json::json!({ "trigger": t }));
            self.emit_turn_ended(
                crate::session::events::TurnOutcomeLabel::Cancelled,
                Some(crate::session::events::CancellationCategory::MidTurnAbort),
                cancellation_context,
            );
            // Mark the next real user prompt as following a mid-turn abort so
            // replay/analytics/the model can see the user stopped this turn.
            // Send-now is a silent cancel-and-send — the user is continuing,
            // not aborting — so it must not arm the interrupt category or the
            // "[Request interrupted]" reminder for its own continuation turn
            // (mirrors the cancel-rate skip above).
            let send_now = trigger.as_deref() == Some("send_now");
            if !send_now {
                self.events.set_prior_interrupt_category(
                    crate::session::events::CancellationCategory::MidTurnAbort,
                );
            }
            // Arm a one-shot `<system-reminder>` for the next real user turn,
            // but only when the abort leaves the model with NO other signal:
            // the partial assistant text is discarded out-of-band, so the only
            // remaining cue is the dangling-tool-call repair. If a tool call is
            // committed but unanswered — a tool mid-execution, OR a turn parked
            // on a permission prompt (where no tool is marked active yet) — the
            // next-turn repair already emits a "cancelled" tool-result, so we
            // skip the reminder to avoid a duplicate signal. Gating on the
            // actual dangling state (not `had_active_tool`) covers the
            // permission-prompt and partial-parallel-call cases too.
            if !send_now && !self.chat_state_handle.has_dangling_tool_calls().await {
                self.events.set_pending_interrupt_reminder();
            }
            // Shared `redirect_kind` for the data pipeline: the next user turn's
            // `turn_started` records HOW the user redirected after this abort.
            self.events
                .set_prior_redirect_kind(if had_queued_user_prompt {
                    crate::session::events::RedirectKind::QueuedAfterCancel
                } else {
                    crate::session::events::RedirectKind::CancelThenSend
                });
        }

        if let Some(running_task) = running_task {
            running_task.abort();
        }
        if let Some(is_turn_active) = &self.tool_context.is_turn_active {
            is_turn_active.store(false, std::sync::atomic::Ordering::Relaxed);
        }
        // The aborted turn's `BlockingWaitGuard`s drop asynchronously (they
        // live in tool futures owned by the drainer task / subagent spawn
        // task). Until they do, `queue_input` would read a stale depth > 0 and
        self.tool_context.blocking_wait_depth.reset();
        self.flush_pending_skill_reminders().await;

        // No multi-second drain here (actor loop would block RecordSubagentUsage).
        // Same UsageDrainOutcome policy as freeze via finalize_usage_from_outcome.
        let cancelled_usage = if rewound_input.is_none() {
            if let Some(ref prompt_id) = cancelled_prompt_id {
                let reply = self.outstanding_reply_for_prompt(prompt_id).await;
                let outcome =
                    super::turn::UsageDrainOutcome::from_outstanding_reply(reply.as_ref());
                self.finalize_usage_from_outcome(prompt_id, outcome).await
            } else if !pending_inputs.is_empty() {
                self.snapshot_prompt_usage().await
            } else {
                None
            }
        } else {
            None
        };
        {
            let mut current_prompt_id = self
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned");
            *current_prompt_id = None;
        }
        if rewound_input.is_none()
            && let Some(prompt_id) = cancelled_prompt_id
        {
            // Stamp `cancelTrigger` on the terminal `_meta` so clients can tell
            // a send-now cancel from an interactive Ctrl+C/Esc.
            self.emit_turn_completed(
                prompt_id,
                &Ok(acp::StopReason::Cancelled),
                cancelled_usage.clone(),
                trigger.as_deref(),
            )
            .await;
        }

        if let Some(input) = rewound_input {
            if let Some(mut snapshot) = self.chat_state_handle.snapshot().await {
                let target_prompt_index = snapshot.prompt_index.saturating_sub(1);
                snapshot.prompt_index = target_prompt_index;
                snapshot.prompt_texts.truncate(target_prompt_index);
                // Cut at the rewound turn's user item (target 0 keeps only
                // the preamble). Marker-first matters here: unlike
                // handle_rewind, this path never replays, so post-compaction
                // conversations rely on the marker for a correct cut.
                let keep_count =
                    conversation_truncate_for_prompt(&snapshot.conversation, target_prompt_index);
                snapshot.conversation.truncate(keep_count);
                self.chat_state_handle.restore_snapshot(snapshot);
                self.file_state_tracker
                    .truncate_from(target_prompt_index)
                    .await;
            }
            let _ = input.respond_to.send(Ok(PromptTurnOk {
                stop_reason: acp::StopReason::Cancelled,
                total_tokens,
                turn_snapshot: None,
                completion_kind: PromptCompletionKind::Rewound,
                structured_output: None,
                usage: None,
                tool_overrides: self.effective_tool_overrides(),
            }));
            return;
        }

        for (idx, input) in pending_inputs.into_iter().enumerate() {
            // Running turn is idx 0; queued prompts never spent tokens.
            let is_running_turn = idx == 0;
            if let Some(task_id) = input.origin.completion_id()
                && let Some(reservations) = &self.tool_context.task_completion_reservations
            {
                reservations.release(task_id);
            }
            let _ = input
                .respond_to
                .send(Ok(PromptTurnOk {
                    stop_reason: acp::StopReason::Cancelled,
                    total_tokens,
                    turn_snapshot: None,
                    completion_kind: PromptCompletionKind::Cancelled {
                        // Previously hard-coded `None`, which dropped the
                        // category on the abort path so `streaming_partial.json`
                        // recorded a bare `"cancelled"`. Carry `MidTurnAbort`
                        // so the partial's `reason` and any downstream consumer
                        // match what `emit_turn_ended` wrote to `events.jsonl`.
                        category: Some(crate::session::events::CancellationCategory::MidTurnAbort),
                        // Thread the trigger on the running turn only (idx 0);
                        // MvpAgent stamps it on the `PromptResponse` `_meta`.
                        context: if is_running_turn {
                            trigger
                                .clone()
                                .map(|t| crate::session::commands::CancellationContext {
                                    trigger: Some(t),
                                    ..Default::default()
                                })
                        } else {
                            None
                        },
                    },
                    structured_output: None,
                    usage: if is_running_turn {
                        cancelled_usage.clone()
                    } else {
                        None
                    },
                    // Only the running turn (idx 0) ran, so only it echoes a bound; a queued prompt
                    // that never promoted attests nothing (like respond_removed_prompt).
                    tool_overrides: if is_running_turn {
                        self.effective_tool_overrides()
                    } else {
                        None
                    },
                }))
                .ok();
        }
    }
}

#[cfg(test)]
mod task_slot_tests {
    // Exercises the shared `TaskSlot<T>` primitive that backs both the deferred
    // prefix and the idle-notification debounce: arm / take / cancel / re-arm.
    use super::TaskSlot;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Spawn a 60s task that bumps `fired` by `by`, armed into `slot`.
    fn arm_counter(slot: &TaskSlot<()>, fired: &Arc<AtomicUsize>, by: usize) {
        let f = Arc::clone(fired);
        slot.arm(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            f.fetch_add(by, Ordering::SeqCst);
        }));
    }

    /// A task left armed runs to completion once its delay elapses.
    #[tokio::test(start_paused = true)]
    async fn armed_task_fires_after_delay() {
        let fired = Arc::new(AtomicUsize::new(0));
        let slot: TaskSlot<()> = TaskSlot::new();
        arm_counter(&slot, &fired, 1);

        let task = slot.take().expect("task armed");
        tokio::time::advance(Duration::from_secs(61)).await;
        let _ = task.await;

        assert_eq!(fired.load(Ordering::SeqCst), 1, "armed task must fire");
    }

    /// `cancel()` aborts a still-pending task (the new-user-prompt reset path).
    #[tokio::test(start_paused = true)]
    async fn cancel_aborts_pending_task() {
        let fired = Arc::new(AtomicUsize::new(0));
        let slot: TaskSlot<()> = TaskSlot::new();
        arm_counter(&slot, &fired, 1);

        tokio::time::advance(Duration::from_secs(30)).await;
        slot.cancel();
        assert!(slot.take().is_none(), "cancel must clear the slot");
        tokio::time::advance(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;

        assert_eq!(
            fired.load(Ordering::SeqCst),
            0,
            "cancelled task must not fire"
        );
    }

    /// Arming a new task aborts the previous one so only the latest fires.
    #[tokio::test(start_paused = true)]
    async fn rearm_aborts_previous_task() {
        let fired = Arc::new(AtomicUsize::new(0));
        let slot: TaskSlot<()> = TaskSlot::new();
        arm_counter(&slot, &fired, 1);
        arm_counter(&slot, &fired, 10);

        let task = slot.take().expect("task armed");
        tokio::time::advance(Duration::from_secs(61)).await;
        let _ = task.await;
        tokio::task::yield_now().await;

        assert_eq!(
            fired.load(Ordering::SeqCst),
            10,
            "only the re-armed task fires"
        );
    }
}
