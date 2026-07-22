//! Idle-gated pending-notification buffering and drain for `SessionActor`,
//! plus auto-start of queued prompts (`maybe_start_running_task`).

use super::*;

/// Maximum number of pending notifications before oldest are dropped.
pub(super) const MAX_PENDING_NOTIFICATIONS: usize = 50;

/// A notification buffered for idle-gated drain (see `maybe_drain_notifications`).
pub(crate) struct PendingNotification {
    #[expect(
        dead_code,
        reason = "Retained for debugging / future per-notification tracing."
    )]
    pub(crate) prompt_id: String,
    pub(crate) prompt_blocks: Vec<acp::ContentBlock>,
    pub(crate) priority: NotificationPriority,
    pub(crate) source: NotificationSource,
}

impl SessionActor {
    pub(super) fn push_pending_notification(state: &mut State, notification: PendingNotification) {
        state.pending_notifications.push(notification);
        let excess = state
            .pending_notifications
            .len()
            .saturating_sub(MAX_PENDING_NOTIFICATIONS);
        if excess > 0 {
            state.pending_notifications.drain(..excess);
            tracing::warn!(
                dropped = excess,
                "Dropped oldest pending notifications (exceeded cap of {})",
                MAX_PENDING_NOTIFICATIONS,
            );
        }
    }

    pub(super) fn push_task_wake_fallback(state: &mut State, fallback: TaskWakeFallback) {
        Self::push_pending_notification(
            state,
            PendingNotification {
                prompt_id: fallback.prompt_id,
                prompt_blocks: fallback.prompt_blocks,
                priority: NotificationPriority::Later,
                source: fallback.source,
            },
        );
    }

    pub(super) async fn consume_deferred_completions(&self) -> Vec<String> {
        let mut state = self.state.lock().await;
        self.sweep_monitor_buffer_into_pending(&mut state, "monitor-user-start-drain");
        let mut completion_ids: Vec<String> = state
            .pending_notifications
            .iter()
            .filter_map(|notification| match &notification.source {
                NotificationSource::BashTaskCompleted { task_id }
                | NotificationSource::MonitorCompleted { task_id } => Some(task_id.clone()),
                NotificationSource::MonitorEvent { .. } => None,
            })
            .collect();
        completion_ids.sort();
        completion_ids.dedup();
        let deferred_ids: std::collections::HashSet<&str> =
            completion_ids.iter().map(String::as_str).collect();

        let notifications = std::mem::take(&mut state.pending_notifications);
        let mut deferred = Vec::new();
        let mut retained = Vec::new();
        for notification in notifications {
            let consume = match &notification.source {
                NotificationSource::BashTaskCompleted { .. }
                | NotificationSource::MonitorCompleted { .. } => true,
                NotificationSource::MonitorEvent { task_id } => {
                    deferred_ids.contains(task_id.as_str())
                }
            };
            if consume {
                deferred.push(notification);
            } else {
                retained.push(notification);
            }
        }
        state.pending_notifications = retained;

        let completion_blocks =
            Self::notification_blocks(&deferred, &self.tool_context.task_output_tool_name);
        drop(state);

        let completion_text = completion_blocks
            .into_iter()
            .filter_map(|block| match block {
                acp::ContentBlock::Text(text) => Some(text.text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !completion_text.is_empty() {
            self.push_system_reminder(&completion_text);
        }
        let completion_id_refs: Vec<&str> = completion_ids.iter().map(String::as_str).collect();
        self.mark_completions_reported(&completion_id_refs).await;
        completion_ids
    }

    pub(super) async fn consume_deferred_completions_for_user_turn(&self) {
        let consumed = self.consume_deferred_completions().await;
        if let Some(reservations) = &self.tool_context.task_completion_reservations {
            for task_id in consumed {
                reservations.release(&task_id);
            }
        }
    }

    pub(super) async fn maybe_start_running_task(
        self: Arc<Self>,
        completion_tx: mpsc::UnboundedSender<(String, PromptTurnResult)>,
    ) {
        // Fast path under the lock: nothing to promote.
        let may_combine;
        {
            let state = self.state.lock().await;
            if state.running_task.is_some() {
                let queue_depth = state.pending_inputs.len();
                if queue_depth > 0 {
                    xai_grok_telemetry::unified_log::debug(
                        "shell.prompt.start_blocked",
                        Some(self.session_info.id.0.as_ref()),
                        Some(serde_json::json!({
                            "reason": "task_already_running",
                            "queue_depth": queue_depth,
                        })),
                    );
                    tracing::debug!(
                        target: "qtrace",
                        pid = std::process::id(),
                        event = "server_start_blocked",
                        queue_depth,
                        front_prompt_id = state
                            .pending_inputs
                            .front()
                            .map(|i| i.prompt_id.as_str())
                            .unwrap_or(""),
                        session = self.session_info.id.0.as_ref(),
                        "maybe_start_running_task blocked: a turn is already running",
                    );
                }
                return;
            }
            if state.pending_inputs.is_empty() {
                return;
            }
            // A merge needs 2+ queued prompts; sample here so the common
            // single-prompt promote skips the config disk read below.
            may_combine = state.pending_inputs.len() >= 2;
        }

        // Config I/O outside the state lock, and only when a merge is even
        // possible — keeps the single-prompt promote (the common case) off disk.
        let combine_queued = may_combine
            && crate::util::config::load_config()
                .await
                .ui
                .combine_queued_prompts
                .unwrap_or(false);

        let mut state = self.state.lock().await;
        // Re-check after the await gap.
        if state.running_task.is_some() || state.pending_inputs.is_empty() {
            return;
        }

        // Note: Auto-compact is now handled inline during process_conversation_turn,
        // so we no longer need to check for queued auto-compact here.

        // Drop stale workflow-completion synthetic fronts (already reported).
        loop {
            let stale = match state.pending_inputs.front().map(|item| &item.origin) {
                Some(super::PromptOrigin::WorkflowCompleted { completion_id }) => {
                    match completion_id
                        .rsplit_once('-')
                        .and_then(|(run_id, revision)| {
                            revision
                                .parse::<u64>()
                                .ok()
                                .map(|revision| (run_id, revision))
                        }) {
                        Some((run_id, revision)) => {
                            let tracker = self.workflow_tracker().await;
                            !tracker.lock().is_unreported_completion(run_id, revision)
                        }
                        None => true,
                    }
                }
                _ => false,
            };
            if !stale {
                break;
            }
            if let Some(item) = state.pending_inputs.pop_front() {
                Self::respond_removed_prompt(item.respond_to);
            }
        }

        // GC stale edit-holds: an id that is no longer queued (promoted,
        // removed, or whose fire-and-forget `release_edit` was dropped) can
        // never be edited again, so drop it to bound the set over a long session.
        if !state.combine_edit_holds.is_empty() {
            let live: std::collections::HashSet<String> = state
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.clone())
                .collect();
            state.combine_edit_holds.retain(|id| live.contains(id));
        }

        if combine_queued {
            let holds: Vec<String> = state.combine_edit_holds.iter().cloned().collect();
            let skip: Vec<&str> = holds.iter().map(String::as_str).collect();
            SessionActor::combine_front_pending_inputs(&mut state.pending_inputs, &skip);
        }

        // Start the next pending user prompt. Pull all needed fields from the
        // queue head in one `front_mut` scope so we can mutate `state` again
        // (e.g. `rewindable`) without overlapping borrows.
        let (
            persist_ack,
            parsed_prompt_tx,
            prompt_id,
            prompt_blocks,
            prompt_mode,
            trace_gcs_config,
            artifact_tracker,
            client_identifier,
            screen_mode,
            verbatim,
            json_schema,
            origin,
            running_display,
        ) = {
            let Some(front) = state.pending_inputs.front_mut() else {
                return;
            };
            let running_display = SessionActor::running_display_from_item(front);
            (
                front.persist_ack.take(),
                front.parsed_prompt_tx.take(),
                front.prompt_id.clone(),
                front.prompt_blocks.clone(),
                front.prompt_mode,
                front.trace_gcs_config.clone(),
                front.artifact_tracker.clone(),
                front.client_identifier.clone(),
                front.screen_mode.clone(),
                front.verbatim,
                front.json_schema.clone(),
                front.origin.clone(),
                running_display,
            )
        };
        if matches!(origin, super::PromptOrigin::User) {
            if let Some(gate) = &self.tool_context.task_wake_suppressed {
                gate.set(false);
            }
            state.notifications_suppressed = false;
            xai_grok_telemetry::unified_log::info(
                "shell.task_wake.gate_cleared",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({ "reason": "queued_user_promotion" })),
            );
        }
        {
            let mut current_prompt_id = self
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned");
            *current_prompt_id = Some(prompt_id.clone());
        }
        state.rewindable = true;
        self.agent
            .borrow()
            .tool_bridge()
            .update_resource(
                xai_grok_tools::implementations::grok_build::task::types::CurrentPromptIdResource(
                    prompt_id.clone(),
                ),
            )
            .await;

        tracing::debug!(
            target: "qtrace",
            pid = std::process::id(),
            event = "server_promote",
            prompt_id = %prompt_id,
            combined_segs = running_display
                .combined_texts
                .as_ref()
                .map(|v| v.len())
                .unwrap_or(0),
            remaining_queued = state.pending_inputs.len().saturating_sub(1),
            session = self.session_info.id.0.as_ref(),
            "promoting front of pending_inputs to the running turn",
        );
        // Promote broadcast before spawn so clients paint (and arm echo-skip)
        // before the user-message chunk can race in.
        self.broadcast_queue_changed_promoting(&state, running_display);

        state.running_task = Some(AgentTask::new_prompt(
            self.clone(),
            prompt_id,
            prompt_blocks,
            prompt_mode,
            trace_gcs_config,
            artifact_tracker,
            client_identifier,
            screen_mode,
            verbatim,
            json_schema,
            completion_tx,
            persist_ack,
            parsed_prompt_tx,
        ));
    }

    /// Drain pending notifications into a single batched turn, if idle and not suppressed.
    ///
    /// Guards:
    /// - No turn is running (`running_task` is `None`)
    /// - No user prompts are pending (user prompts always take priority)
    /// - Notifications are NOT suppressed (cleared on next user prompt)
    ///
    /// All notifications are taken and merged into a single `InputItem` with
    /// `---` separators between content blocks. The take+push happens in a
    /// single lock acquisition to avoid interleaving.
    pub(super) async fn maybe_drain_notifications(
        self: Arc<Self>,
        completion_tx: mpsc::UnboundedSender<(String, PromptTurnResult)>,
    ) {
        // Auto-wake notification turns are DROPPED both while the goal loop is
        // active (a bg-task / monitor "completed" turn would pull a weak model
        // off the goal continuation, e.g. relaunching a killed server) AND
        // after the goal completes (the autonomous run is over — late dev-
        // server completions should leave the session idle, not spawn fresh
        // post-goal turns). Independently, completions whose source task
        // originated during the goal turn are dropped regardless of status (see
        // `split_goal_suppressed`). Dropped notifications are still marked
        // reported below so nothing resurfaces later.
        let suppress_all = self.goal_harness_enabled()
            && matches!(
                self.goal_tracker.lock().status(),
                Some(
                    crate::session::goal_tracker::GoalStatus::Active
                        | crate::session::goal_tracker::GoalStatus::Complete
                )
            );

        let drained_task_ids: Vec<String>;

        let drained = {
            let mut state = self.state.lock().await;

            // Shared idle predicate — same conditions Layer 3 uses via
            // `is_session_idle_for_injection`. Inlined here so the
            // `mut state` borrow can survive into the take/push below.
            if !is_session_idle_for_injection(&state) {
                return;
            }

            // Backstop sweep for events that hit the buffer after the
            // turn-end drain (the is_turn_active flag can lag the actual
            // turn teardown). Normally a no-op.
            self.sweep_monitor_buffer_into_pending(&mut state, "monitor-idle-drain");

            // Nothing to drain
            if state.pending_notifications.is_empty() {
                return;
            }

            // Take all notifications and build merged blocks inside the lock
            let notifications = std::mem::take(&mut state.pending_notifications);

            drained_task_ids = notifications
                .iter()
                .map(|n| n.source.task_id().to_string())
                .collect();

            let (to_surface, dropped) = {
                let goal_turn_task_ids = self.goal_turn_task_ids.lock();
                Self::split_goal_suppressed(suppress_all, &goal_turn_task_ids, notifications)
            };
            if dropped > 0 {
                tracing::info!(
                    dropped,
                    suppress_all,
                    "dropping suppressed pending notifications (goal active/complete or goal-turn origin)"
                );
            }

            if to_surface.is_empty() {
                false
            } else {
                Self::drain_notifications_into_turn(
                    &mut state,
                    to_surface,
                    &self.tool_context.task_output_tool_name,
                )
            }
        };
        // Mark reported whether dropped or surfaced, so the per-tool-call
        // `TaskCompletionReminder` won't resurface the same completions.
        let ids: Vec<&str> = drained_task_ids.iter().map(String::as_str).collect();
        self.mark_completions_reported(&ids).await;

        if drained {
            SessionActor::maybe_start_running_task(self, completion_tx).await;
        }
    }

    /// Notifies extensions when the session settles idle (nothing running, nothing queued).
    /// The idle check stays host-side; extensions only get the event.
    pub(super) async fn emit_session_idle_if_idle(&self) {
        {
            let state = self.state.lock().await;
            if !is_session_idle_for_injection(&state) {
                return;
            }
        }
        for contributor in self.extension_registry.session_lifecycle_contributors() {
            contributor
                .on_session_idle(&xai_agent_lifecycle::SessionIdleInput)
                .await;
        }
    }

    /// Sweep this session's buffered monitor events (`drain_owned`) into
    /// `pending_notifications`. Used where the turn loop can no longer
    /// drain the buffer: turn end (`drain_monitor_buffer_to_pending`),
    /// turn cancel, and the idle drain (all three race the
    /// `is_turn_active`-gated buffer push in `InjectNotification`).
    pub(super) fn sweep_monitor_buffer_into_pending(
        &self,
        state: &mut State,
        prompt_id_prefix: &str,
    ) {
        let Some(buffer) = &self.tool_context.monitor_event_buffer else {
            return;
        };
        for event in xai_grok_tools::implementations::grok_build::task::types::drain_owned(
            buffer,
            Some(self.session_info.id.0.as_ref()),
        ) {
            Self::push_pending_notification(
                state,
                PendingNotification {
                    prompt_id: format!("{prompt_id_prefix}-{}", uuid::Uuid::now_v7()),
                    prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                        event.event_text,
                    ))],
                    priority: NotificationPriority::Next,
                    source: NotificationSource::MonitorEvent {
                        task_id: event.task_id,
                    },
                },
            );
        }
    }

    /// Partition drained notifications into `(to_surface, dropped_count)`.
    ///
    /// `suppress_all` mirrors the goal Active/Complete blanket gate (drop
    /// everything); independently, notifications whose source task is in
    /// `goal_turn_task_ids` are always dropped (see that field).
    pub(super) fn split_goal_suppressed(
        suppress_all: bool,
        goal_turn_task_ids: &std::collections::HashSet<String>,
        notifications: Vec<PendingNotification>,
    ) -> (Vec<PendingNotification>, usize) {
        if suppress_all {
            let dropped = notifications.len();
            return (Vec::new(), dropped);
        }
        let mut dropped = 0usize;
        let to_surface = notifications
            .into_iter()
            .filter(|n| {
                let keep = !goal_turn_task_ids.contains(n.source.task_id());
                if !keep {
                    dropped += 1;
                }
                keep
            })
            .collect();
        (to_surface, dropped)
    }

    fn notification_blocks(
        notifications: &[PendingNotification],
        task_output_tool_name: &str,
    ) -> Vec<acp::ContentBlock> {
        use xai_grok_tools::implementations::grok_build::task::types::MonitorEventNotification;

        let completion_task_ids: std::collections::HashSet<&str> = notifications
            .iter()
            .filter_map(|notification| match &notification.source {
                NotificationSource::MonitorCompleted { task_id } => Some(task_id.as_str()),
                NotificationSource::MonitorEvent { .. }
                | NotificationSource::BashTaskCompleted { .. } => None,
            })
            .collect();
        let mut monitor_events: Vec<MonitorEventNotification> = Vec::new();
        let mut sections: Vec<Vec<acp::ContentBlock>> = Vec::new();
        let mut monitor_section_idx: Option<usize> = None;
        for notification in notifications {
            match &notification.source {
                NotificationSource::MonitorEvent { task_id } => {
                    if completion_task_ids.contains(task_id.as_str()) {
                        continue;
                    }
                    let event_text = notification
                        .prompt_blocks
                        .iter()
                        .filter_map(|block| match block {
                            acp::ContentBlock::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    monitor_events.push(MonitorEventNotification {
                        task_id: task_id.clone(),
                        event_text,
                        owner_session_id: None,
                    });
                    if monitor_section_idx.is_none() {
                        monitor_section_idx = Some(sections.len());
                        sections.push(Vec::new());
                    }
                }
                NotificationSource::MonitorCompleted { .. }
                | NotificationSource::BashTaskCompleted { .. } => {
                    sections.push(notification.prompt_blocks.clone());
                }
            }
        }
        if let (Some(index), Some(batch)) = (
            monitor_section_idx,
            xai_grok_tools::reminders::task_completion::format_monitor_events(
                &monitor_events,
                Some(task_output_tool_name),
            ),
        ) {
            sections[index] = vec![acp::ContentBlock::Text(acp::TextContent::new(batch))];
        }

        let mut blocks = Vec::new();
        for (index, section) in sections.iter().enumerate() {
            if index > 0 {
                blocks.push(acp::ContentBlock::Text(acp::TextContent::new("---")));
            }
            blocks.extend(section.iter().cloned());
        }
        blocks
    }

    /// Merge notifications into one queued `NotificationDrain` turn.
    pub(super) fn drain_notifications_into_turn(
        state: &mut State,
        notifications: Vec<PendingNotification>,
        task_output_tool_name: &str,
    ) -> bool {
        let merged_blocks = Self::notification_blocks(&notifications, task_output_tool_name);

        let merged_prompt_id = format!("notifications-{}", uuid::Uuid::now_v7());

        // Receiver intentionally dropped — notification turns have no caller
        // awaiting the result. The send() in handle_completion returns Err,
        // which is harmless.
        let (respond_to, _) = tokio::sync::oneshot::channel();

        state.pending_inputs.push_back(InputItem {
            prompt_id: merged_prompt_id,
            prompt_blocks: merged_blocks,
            prompt_mode: crate::session::plan_mode::PromptMode::Agent,
            trace_gcs_config: None,
            artifact_tracker: None,
            client_identifier: None,
            screen_mode: None,
            verbatim: true,
            json_schema: None,
            origin: super::PromptOrigin::NotificationDrain,
            task_wake_fallback: None,
            respond_to,
            persist_ack: None,
            parsed_prompt_tx: None,
            queue_meta: None,
            send_now: false,
        });

        tracing::info!(
            count = notifications.len(),
            next_count = notifications.iter().filter(|n| n.priority == NotificationPriority::Next).count(),
            later_count = notifications.iter().filter(|n| n.priority == NotificationPriority::Later).count(),
            sources = %notifications.iter().map(|n| match &n.source {
                NotificationSource::MonitorEvent { task_id } => format!("monitor:{task_id}"),
                NotificationSource::MonitorCompleted { task_id } => format!("monitor-completed:{task_id}"),
                NotificationSource::BashTaskCompleted { task_id } => format!("bash:{task_id}"),
            }).collect::<Vec<_>>().join(","),
            "Drained pending notifications into single batched turn"
        );

        true
    }

    /// Turn-end straggler sweep: monitor events buffered during the turn's
    /// final sampling step (after the loop's last `inject_pending_monitor_events`
    /// pass) move to `pending_notifications`. Runs in the completion handler
    /// before `maybe_drain_notifications`, so it — not the idle sweep — is
    /// what normally catches them.
    pub(super) async fn drain_monitor_buffer_to_pending(&self) {
        let mut state = self.state.lock().await;
        self.sweep_monitor_buffer_into_pending(&mut state, "monitor-turn-end-drain");
    }
}
