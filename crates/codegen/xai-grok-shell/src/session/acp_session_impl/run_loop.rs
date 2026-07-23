//! The session actor's main loop (`run_session`): command dispatch, idle
//! arms, and the free helpers only the loop consumes.
#![allow(clippy::items_after_test_module)]
use super::*;
/// The `YoloToggled` event to emit after `set_yolo_mode(requested)`, given the
/// previous state and the post-call ACTUAL state (read back via
/// `is_yolo_mode()`). Returns `Some(actual)` only on a real change.
///
/// Callers MUST pass the read-back `actual`, never the request: under the
/// always-approve pin the manager clamps a requested ON to OFF, so reporting the
/// request would announce (event + telemetry + log) a turn-on that never
/// happened.
pub(super) fn yolo_toggle_report(was: bool, actual: bool) -> Option<bool> {
    (was != actual).then_some(actual)
}
#[cfg(test)]
mod yolo_toggle_report_tests {
    use super::yolo_toggle_report;
    /// A pin-clamped enable (requested ON but actual stays OFF) reports no
    /// change, so no spurious "turned on" event/telemetry is emitted. Real
    /// flips report the actual new state.
    #[test]
    fn reports_actual_state_change_only() {
        assert_eq!(yolo_toggle_report(false, false), None);
        assert_eq!(yolo_toggle_report(false, true), Some(true));
        assert_eq!(yolo_toggle_report(true, false), Some(false));
        assert_eq!(yolo_toggle_report(true, true), None);
    }
}
/// Best-effort removal of this session's per-session scratch staging on
/// teardown. A no-op in builds without a scratch producer.
fn cleanup_session_scratch(_session: &SessionActor) {}
impl SessionActor {
    /// Serialize terminal task-wake admission with interactive cancellation.
    pub(super) async fn admit_task_completion_wake(
        &self,
        origin: &super::PromptOrigin,
        admission: TaskWakeAdmission,
    ) -> Option<TaskWakeFallback> {
        let TaskWakeAdmission {
            respond_to,
            fallback,
        } = admission;
        let super::PromptOrigin::TaskCompleted { task_id } = origin else {
            return respond_to.send(true).is_ok().then_some(fallback);
        };
        let gate_suppressed = self
            .tool_context
            .task_wake_suppressed
            .as_ref()
            .is_some_and(|gate| gate.get());
        let mut state = self.state.lock().await;
        let state_suppressed = state.notifications_suppressed;
        let admitted = !gate_suppressed && !state_suppressed;
        if !admitted {
            Self::push_task_wake_fallback(&mut state, fallback);
            drop(state);
            xai_grok_telemetry::unified_log::info(
                "shell.task_wake.actor_admission",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({
                    "task_id": task_id,
                    "gate": gate_suppressed,
                    "state": state_suppressed,
                    "admitted": false,
                })),
            );
            let _ = respond_to.send(false);
            return None;
        }
        if respond_to.send(true).is_err() {
            Self::push_task_wake_fallback(&mut state, fallback);
            return None;
        }
        drop(state);
        xai_grok_telemetry::unified_log::info(
            "shell.task_wake.actor_admission",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "task_id": task_id,
                "gate": gate_suppressed,
                "state": state_suppressed,
                "admitted": true,
            })),
        );
        Some(fallback)
    }
}
async fn shutdown_workflows(session: &SessionActor) {
    if let Err(run_ids) = session
        .workflow_manager
        .lock()
        .await
        .cancel_all_and_drain(std::time::Duration::from_secs(7))
        .await
    {
        tracing::warn!(
            ?run_ids,
            "workflow shutdown completed with interrupted runs"
        );
    }
    let (respond_to, ack) = tokio::sync::oneshot::channel();
    if session
        .notifications
        .persistence_tx
        .send(PersistenceMsg::FlushAndAck { respond_to })
        .is_err()
    {
        tracing::warn!("workflow shutdown persistence channel closed before flush");
        return;
    }
    match tokio::time::timeout(std::time::Duration::from_secs(2), ack).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            tracing::warn!("workflow shutdown persistence actor dropped flush ack")
        }
        Err(_) => tracing::warn!("workflow shutdown persistence flush timed out"),
    }
}
pub(super) async fn run_session(
    session: Arc<SessionActor>,
    mut cmd_rx: mpsc::UnboundedReceiver<SessionCommand>,
    mut chat_state_event_rx: mpsc::UnboundedReceiver<xai_chat_state::ChatStateEvent>,
    mut event_rx: mpsc::UnboundedReceiver<SessionEvent>,
    fs_notify_config: Option<ClientFsConfig>,
    codebase_indexes: std::sync::Arc<parking_lot::Mutex<CodebaseIndexManager>>,
    index_root: std::path::PathBuf,
    fs_watch_caps: fs_watch::FsWatchCapabilities,
) {
    let (completion_tx, mut completion_rx) =
        mpsc::unbounded_channel::<(String, PromptTurnResult)>();
    tracing::debug!("fs_notify_config: {:?}", fs_notify_config);
    let mut replay_buffer = ReplayBuffer::new(session.buffering_settings.clone());
    let event_tx_for_flush_timer = session.event_tx.clone();
    let buffering_flush_interval = replay_buffer.max_wait_duration_ms();
    if let Some(buffering_flush_interval) = buffering_flush_interval {
        tokio::task::spawn_local(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(std::cmp::max(
                20,
                buffering_flush_interval * 2,
            )));
            loop {
                interval.tick().await;
                let _ =
                    event_tx_for_flush_timer.send(SessionEvent::FlushReplay { respond_to: None });
            }
        });
    }
    let _workflow_watch = crate::config::watcher::ProjectDiscoveryWatcher::start(
        std::path::Path::new(session.session_info.cwd.as_str()),
    )
    .map(|(mut watcher, mut changes)| {
        let session = session.clone();
        tokio::task::spawn_local(async move {
            while let Some(change) = changes.recv().await {
                watcher.refresh_new_dirs();
                match change {
                    crate::config::watcher::DiscoveryChange::Skills => {
                        session.reload_skills_from_disk().await;
                    }
                    crate::config::watcher::DiscoveryChange::Workflows => {
                        session.send_available_commands_update().await;
                    }
                }
            }
        })
    });
    let _fs_watch: Option<fs_watch::FsWatchHandle> = if fs_watch_caps.needs_watcher() {
        let deps = fs_watch::FsWatchDeps::from_session(
            &session,
            fs_notify_config.clone(),
            codebase_indexes.clone(),
            index_root.clone(),
        );
        tracing::debug!(?fs_watch_caps, "fs-notify: spawning");
        Some(fs_watch::spawn(fs_watch::FsWatchPlan::build(
            fs_watch_caps,
            deps,
        )))
    } else {
        tracing::debug!("fs-notify: skipped (no consumers)");
        None
    };
    {
        let s = session.clone();
        tokio::task::spawn_local(async move { s.maybe_notify_git_branch().await });
    }
    let liveness_watchers_enabled = {
        let user_cfg = crate::config::load_effective_config().ok();
        let requirements = crate::agent::config::read_requirements_toml();
        crate::util::config::resolve_mcp_liveness_watchers(
            requirements.as_ref(),
            user_cfg.as_ref(),
            None,
        )
    };
    if !session.startup_hints.is_subagent && liveness_watchers_enabled {
        let (event_tx, event_rx) =
            tokio::sync::mpsc::unbounded_channel::<xai_grok_mcp::servers::McpClientEvent>();
        {
            let mut mcp_state = session.mcp_state.lock().await;
            mcp_state.set_client_event_tx(Some(event_tx));
        }
        let dispatcher_session_id = session.session_info.id.0.to_string();
        let dispatcher_cwd = std::path::PathBuf::from(session.session_info.cwd.as_str());
        let dispatcher_gateway = session.notifications.gateway.clone();
        let dispatcher_mcp_state = Arc::clone(&session.mcp_state);
        let shutdown_state = crate::session::mcp_dispatcher::new_shutdown_state();
        let auto_restart_enabled = {
            let user_cfg = crate::config::load_effective_config().ok();
            let requirements = crate::agent::config::read_requirements_toml();
            crate::util::config::resolve_mcp_auto_restart(
                requirements.as_ref(),
                user_cfg.as_ref(),
                None,
            )
        };
        let restart_actions: Option<std::rc::Rc<dyn crate::session::mcp_restart::RestartActions>> =
            if auto_restart_enabled {
                Some(std::rc::Rc::new(SessionRestartActions::new(
                    session.clone(),
                    Arc::clone(&shutdown_state),
                )))
            } else {
                None
            };
        tokio::task::spawn_local(async move {
            crate::session::mcp_dispatcher::run_dispatcher(
                dispatcher_session_id,
                event_rx,
                dispatcher_gateway,
                dispatcher_mcp_state,
                shutdown_state,
                restart_actions,
                dispatcher_cwd,
            )
            .await;
        });
    }
    let session_for_mcp = session.clone();
    let completion_tx_for_mcp = completion_tx.clone();
    tokio::task::spawn_local(async move {
        session_for_mcp.ensure_mcp_tools_initialized().await;
        SessionActor::maybe_start_running_task(session_for_mcp.clone(), completion_tx_for_mcp)
            .await;
    });
    let mut model_switch_rx = session.models_manager.subscribe_model_switch();
    let _ = *model_switch_rx.borrow_and_update();
    let idle_flush_sleep = match session.idle_flush_timeout {
        Some(timeout) => tokio::time::sleep(timeout),
        None => tokio::time::sleep(std::time::Duration::MAX),
    };
    tokio::pin!(idle_flush_sleep);
    let dream_check_sleep = match session.dream_check_timeout {
        Some(timeout) => tokio::time::sleep(timeout),
        None => tokio::time::sleep(std::time::Duration::MAX),
    };
    tokio::pin!(dream_check_sleep);
    loop {
        tokio::select! {
                biased;
                // Idle flush timer fired — run background flush.
                _ = &mut idle_flush_sleep, if session.idle_flush_timeout.is_some()
                    && session.memory.is_enabled()
                    && !session.memory.is_flushing.load(std::sync::atomic::Ordering::Relaxed) => {
                    // Skip if no new messages since last idle flush
                    let current_len = session.chat_state_handle.get_conversation_len().await;
                    let last_len = session.last_idle_flush_conversation_len
                        .load(std::sync::atomic::Ordering::Relaxed);
                    if current_len > last_len {
                        tracing::info!(target: xai_grok_telemetry::memory_log::TARGET,
                            "MEMORY_IDLE_FLUSH: timer fired (conversation {last_len} → {current_len})");
                        session.last_idle_flush_conversation_len
                            .store(current_len, std::sync::atomic::Ordering::Relaxed);
                        tokio::task::spawn_local({
                            let session = session.clone();
                            async move {
                                if !session.run_memory_flush("interval", None).await {
                                    tracing::info!(target: xai_grok_telemetry::memory_log::TARGET,
                                        "MEMORY_IDLE_FLUSH: skipped — another flush already in progress");
                                }
                            }
                        });
                    } else {
                        tracing::debug!(target: xai_grok_telemetry::memory_log::TARGET,
                            "MEMORY_IDLE_FLUSH: skipped, no new messages since last flush (len={current_len})");
                    }
                    // Reset for next idle period
                    if let Some(timeout) = session.idle_flush_timeout {
                        idle_flush_sleep.as_mut().reset(tokio::time::Instant::now() + timeout);
                    }
                }
                // Dream check timer — periodically run dream consolidation.
                _ = &mut dream_check_sleep, if session.dream_check_timeout.is_some()
                    && session.memory.is_enabled() => {
                    tracing::debug!(target: xai_grok_telemetry::memory_log::TARGET,
                        "MEMORY_DREAM_CHECK: timer fired");
                    tokio::task::spawn_local({
                        let session = session.clone();
                        async move {
                            session.maybe_run_dream().await;
                        }
                    });
                    if let Some(timeout) = session.dream_check_timeout {
                        dream_check_sleep.as_mut().reset(tokio::time::Instant::now() + timeout);
                    }
                }
                // Layer-3 LazinessDetector: zero the per-session nudge
                // counter whenever the user switches models. The cap
                // is per-(session, model) — switching is a deliberate
                // user action that resets expectations. `.changed()`
                // only resolves on switches AFTER subscription, so
                // there is no stored-permit hazard.
                changed = model_switch_rx.changed() => {
                    if changed.is_ok() {
                        let new_gen = *model_switch_rx.borrow_and_update();
                        session.handle_model_switch_for_laziness(new_gen).await;
                    }
                }
                // ChatStateActor events — coordination signals for session-level concerns.
                event = chat_state_event_rx.recv() => {
                    match event {
                        Some(xai_chat_state::ChatStateEvent::ConversationReset { new_len }) => {
                            // Reset idle-flush counter so next idle period flushes the new state.
                            session.last_idle_flush_conversation_len
                                .store(new_len, std::sync::atomic::Ordering::Relaxed);
                            // Re-arm the first-turn injection check after
                            // compaction (re-search only if no block persisted).
                            session.memory.context_injected
                                .store(false, std::sync::atomic::Ordering::Relaxed);
                        }
                        Some(xai_chat_state::ChatStateEvent::ImageBudget {
                            body_bytes,
                            trigger_bytes,
                            reclaim_target_bytes,
                            inline_images,
                            needs_image_compaction,
                            evicted,
                            body_bytes_after,
                        }) => {
                            // Unified-log record for local image-eviction verification.
                            xai_grok_telemetry::unified_log::info(
                                "shell.image_budget",
                                Some(session.session_info.id.0.as_ref()),
                                Some(serde_json::json!({
                                    "body_bytes": body_bytes,
                                    "body_bytes_after": body_bytes_after,
                                    "trigger_bytes": trigger_bytes,
                                    "reclaim_target_bytes": reclaim_target_bytes,
                                    "inline_images": inline_images,
                                    "images_remaining": inline_images.saturating_sub(evicted),
                                    "needs_image_compaction": needs_image_compaction,
                                    "evicted": evicted,
                                })),
                            );
                        }
                        Some(xai_chat_state::ChatStateEvent::PromptIndexChanged { .. }) |
                        Some(xai_chat_state::ChatStateEvent::TokensUpdated { .. }) => {
                            // Prompt index and token updates are informational —
                            // consumers query the actor directly when they need them.
                        }
                        None => {
                            // Actor shut down — no more events.
                        }
                    }
                }
                maybe_event = event_rx.recv() => {
                    if let Some(event) = maybe_event {
                        match event {
                            SessionEvent::Notification(notification) => {
                                let out = replay_buffer.consume_chunk(notification);
                                match out {
                                    None => {}
                                    Some((first, second)) => {
                                        session.emit_buffered(first).await;
                                        if let Some(second) = second {
                                            session.emit_buffered(second).await;
                                        }
                                    }
                                }
                            }
                            SessionEvent::FlushReplay { respond_to } => {
                                if let Some(notification) = replay_buffer.flush() {
                                    session.emit_buffered(notification).await;
                                }

                                // Always ack (independent of whether anything was buffered).
                                if let Some(tx) = respond_to {
                                    let _ = tx.send(());
                                }
                            }
                        }
                    }
                }
                maybe_completion = completion_rx.recv() => {
                    let Some((prompt_id, result)) = maybe_completion else {
                        // Channel closed - shutdown feedback sync loop
                        shutdown_workflows(&session).await;
                        if let Some(cancel) = &session.sync_loop_cancel {
                            cancel.cancel();
                        }
                        cleanup_session_scratch(&session);
                        return;
                    };
                    // Flush any buffered turn deltas before `handle_completion`
                    // emits the durable `TurnCompleted`, so the terminal lands
                    // in updates.jsonl strictly after the turn's last
                    // `session/update` delta. Mirrors the Cancel / Shutdown /
                    // FlushComplete arms.
                    if let Some(notification) = replay_buffer.flush() {
                        session.emit_buffered(notification).await;
                    }
                    let (turn_succeeded, infra_pause_message) =
                        SessionActor::post_turn_goal_degradation_plan(&result);
                    session.handle_completion(prompt_id, result).await;
                    // Drain any monitor events that were routed to the mid-turn buffer
                    // but arrived after the turn ended (race between is_turn_active and buffer push).
                    session.drain_monitor_buffer_to_pending().await;
                    if let Some(message) = infra_pause_message {
                        session.apply_infra_pause_after_turn_err(message).await;
                    }
                    // Goal continuation (success) or back-off (non-success).
                    // Owns the streak-tracking and reminder-injection path.
                    session.handle_turn_end(turn_succeeded).await;
                    // Interjections that raced past the turn's final drain
                    // (arrived during turn-end bookkeeping) have no turn left
                    // to merge into — convert them to front-of-queue prompt
                    // turns so the message runs instead of stranding.
                    //
                    // INVARIANT: this flush must only ever see interjections
                    // aimed at the turn that just completed. That holds
                    // because this arm runs in the same serialized actor loop
                    // as `SessionCommand::Interject` (no live turn's buffer
                    // can be stolen mid-stream), and the Cancel arm clears
                    // the buffer before its completion arrives. If the
                    // select arms are ever reordered or the Cancel clear
                    // moves, re-audit this flush.
                    if session.flush_stranded_interjections().await {
                        tracing::info!("Flushed stranded interjection(s) into prompt turns");
                    }
                    SessionActor::maybe_start_running_task(session.clone(), completion_tx.clone()).await;
                    // If no user prompt started, check for pending notifications
                    SessionActor::maybe_drain_notifications(session.clone(), completion_tx.clone()).await;
                    session.emit_session_idle_if_idle().await;
                    // Layer-3 LazinessDetector: spawn an idle-triggered
                    // classifier dispatch. The method is a no-op when the
                    // per-model `laziness_detector.enabled = false`
                    // (the v1 default for every model), so no
                    // classification cost is incurred without explicit
                    // opt-in. Spawned via `spawn_local` so the actor
                    // loop can continue accepting commands while the
                    // classifier idle-waits.
                    {
                        let s = session.clone();
                        tokio::task::spawn_local(async move {
                            s.maybe_fire_laziness_check().await;
                        });
                    }
                }
                maybe_cmd = cmd_rx.recv() => {
                    let Some(cmd) = maybe_cmd else {
                        // ── session_end hook (channel-closed path) ────
                        // Fires BEFORE memory auto-save per plan contract.
                        let envelope = session.fire_hook(
                            xai_grok_hooks::event::HookEventName::SessionEnd,
                            None,
                            xai_grok_hooks::event::HookPayload::SessionEnd {
                                reason: "channel_closed".to_string(),
                                turn_count: None,
                                tool_call_count: None,
                            },
                        );
                        if let Some(registry) = session.hook_registry.borrow().clone() {
                            let ctx = session.hook_run_ctx();
                            let results = xai_grok_hooks::dispatcher::dispatch_non_blocking(
                                &registry,
                                xai_grok_hooks::event::HookEventName::SessionEnd,
                                &envelope,
                                &ctx,
                            )
                            .await;
                            session.send_hook_execution("session_end", None, None, &results).await;
                        }
                        session.dispatch_session_end_stop("channel_closed").await;
                        // Channel closed -- run memory session-end hook.
                        let mut session_end_result = "disabled";
                        let mut total_chunks_at_end = 0usize;
                        if !session.startup_hints.is_subagent {
                            if let Some(storage) = session.memory.storage() {
                                let conversation = session.chat_state_handle.get_conversation().await;
                                let result = crate::session::memory::hooks::on_session_end(
                                    &storage,
                                    &conversation,
                                    &session.session_info.id.0,
                                    session.memory.save_on_end,
                                );
                                session_end_result = match &result {
                                    crate::session::memory::hooks::SessionEndResult::Written(_) => "written",
                                    crate::session::memory::hooks::SessionEndResult::Skipped => "skipped",
                                    crate::session::memory::hooks::SessionEndResult::Failed(_) => "failed",
                                };
                                total_chunks_at_end = storage.total_chunk_count();
                                let telem = session.memory.telemetry_snapshot();
                                tracing::info!(
                                    target: xai_grok_telemetry::memory_log::TARGET,
                                    result = ?result,
                                    tool_searches = telem.tool_search_count,
                                    injection_searches = telem.injection_count,
                                    recovery_searches = telem.compaction_recovery_count,
                                    "MEMORY_SESSION_END: channel closed, session summary saved"
                                );
                                if let crate::session::memory::hooks::SessionEndResult::Written(ref path_str) = result {
                                    session.reindex_and_embed(std::path::Path::new(path_str), "session").await;
                                    session.send_xai_notification(XaiSessionUpdate::MemorySessionSaved {
                                        path: path_str.clone(),
                                    }).await;
                                }
                            }
                        } else {
                            tracing::debug!(
                                target: xai_grok_telemetry::memory_log::TARGET,
                                "MEMORY_SUBAGENT_SKIP: skipping on_session_end for subagent session"
                            );
                        }
                        // Dream: attempt consolidation at session end
                        session.maybe_run_dream().await;
                        // Structured telemetry after dream so counters are populated
                        let telem = session.memory.telemetry_snapshot();
                        session.emit_memory_session_summary(&telem, total_chunks_at_end, session_end_result);
                        if let Some(notification) = replay_buffer.flush() {
                            session.emit_buffered(notification).await;
                        }
                        {
                            let model_id = session.current_model_id().await;
                            if let Some(signals) = session.signals_handle().snapshot().await {
                                xai_grok_telemetry::session_ctx::log_event(
                                    xai_grok_telemetry::events::SessionEnded {
                                        duration_secs: session.session_start.elapsed().as_secs(),
                                        turn_count: signals.turn_count as u64,
                                        tool_call_count: signals.tool_call_count as u64,
                                        compaction_count: signals.compaction_count as u64,
                                        model_id,
                                    },
                                );
                            }
                        }
                        shutdown_workflows(&session).await;
                        if let Some(cancel) = &session.sync_loop_cancel {
                            cancel.cancel();
                        }
                        session.feedback_manager.shutdown(session.upload_queue.get()).await;
                        if !session.startup_hints.is_subagent {
                            session.persist_background_task_manifest().await;
                        }
                        cleanup_session_scratch(&session);
                        return;
                    };

                    match cmd {
                        SessionCommand::Initialize { system_prompt } => {
                            session.initialize(system_prompt).await;
                            let s = session.clone();
                            let handle = tokio::task::spawn_local(async move {
                                s.build_prefix_background().await
                            });
                            session.deferred_prefix.arm(handle);
                        }
                        SessionCommand::ReplaceSystemPrompt { system_prompt } => {
                            session.handle_replace_system_prompt(system_prompt).await;
                        }
                        SessionCommand::RestorePlanApproval => {
                            // Resume re-park: spawn the approval
                            // round-trip so the command loop is not blocked on
                            // the (open-ended) user decision.
                            //
                            // Detaching the handle is safe: the task is spawned on
                            // this session's `LocalSet`, so it is dropped (its
                            // `request_plan_approval` future cancelled, clearing
                            // `awaiting` via the guard) when the session ends — it
                            // cannot outlive the actor. `resume_plan_approval`
                            // also self-guards against a concurrent/duplicate
                            // re-park via the `pending_interactions` registry.
                            let s = session.clone();
                            let completion_tx = completion_tx.clone();
                            tokio::task::spawn_local(async move {
                                s.resume_plan_approval(completion_tx).await;
                            });
                        }
                        SessionCommand::GetToolOverrides { respond_to } => {
                            let _ = respond_to.send(session.effective_tool_overrides());
                        }
                        SessionCommand::SetToolOverrides { overrides } => {
                            session.set_tool_overrides(overrides);
                        }
                        SessionCommand::Prompt { prompt_id, prompt_blocks, prompt_mode, artifact_upload_ctx, client_identifier, screen_mode, verbatim, traceparent, json_schema, send_now, admission, tool_overrides_update, respond_to, persist_ack, parsed_prompt_tx } => {
                            let origin = super::PromptOrigin::from_prompt_id(&prompt_id);
                            let (actor_admitted, task_wake_fallback) = match admission {
                                Some(admission) => {
                                    let fallback = session
                                        .admit_task_completion_wake(&origin, admission)
                                        .await;
                                    (fallback.is_some(), fallback)
                                }
                                None => (true, None),
                            };
                            if !actor_admitted {
                                SessionActor::respond_removed_prompt(respond_to);
                                continue;
                            }
                            session.ensure_prefix_ready().await;
                            // Clear suppression -- user is re-engaging
                            // (skip for synthetic auto-wake prompts; the user hasn't
                            // actually re-engaged, so post-cancel suppression must hold)
                            if !origin.is_synthetic() {
                                if let Some(gate) = &session.tool_context.task_wake_suppressed {
                                    gate.set(false);
                                }
                                let mut state = session.state.lock().await;
                                state.notifications_suppressed = false;
                                xai_grok_telemetry::unified_log::info(
                                    "shell.task_wake.gate_cleared",
                                    Some(session.session_info.id.0.as_ref()),
                                    Some(serde_json::json!({ "reason": "user_intake" })),
                                );
                                // Layer-3 LazinessDetector wake: bump
                                // the monotonic counter so any
                                // currently-spawned classifier
                                // poll-loop snapshots a stale value
                                // and aborts. Synthetic prompts
                                // (NotificationDrain, GoalSummary,
                                // auto-wake) are not real user input
                                // and must NOT bump the counter.
                                // `AcqRel` (not bare `Release`): `fetch_add`
                                // is a read-modify-write — `AcqRel` publishes
                                // our write AND synchronizes the read half,
                                // so any future reader chaining off the
                                // returned counter value sees all prior
                                // writes from other threads. Costs nothing
                                // on x86, costs little on ARM.
                                session
                                    .user_input_generation
                                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                            }
                            if origin.is_synthetic() {
                                let state = session.state.lock().await;
                                let has_running = state.running_task.is_some();
                                let queue_depth = state.pending_inputs.len();
                                drop(state);
                                tracing::info!(
                                    prompt_id = %prompt_id,
                                    has_running_task = has_running,
                                    queue_depth = queue_depth,
                                    "auto-wake: session actor received synthetic prompt"
                                );
                            }
                            // Adopt the caller's trace context so session.handle_prompt
                            // is linked to agent.prompt across the channel boundary.
                            if let Some(ref tp) = traceparent {
                                let meta = serde_json::json!({ "traceparent": tp });
                                xai_file_utils::trace_context::link_current_span_to_meta(&meta);
                            }
                            let (trace_gcs_config, artifact_tracker) = match artifact_upload_ctx {
                                Some(tu) => (Some(tu.gcs_config), Some(tu.artifact_tracker)),
                                None => (None, None),
                            };
                            let cancel_for_send_now = session
                                .queue_input(prompt_blocks, prompt_id, prompt_mode, trace_gcs_config, artifact_tracker, client_identifier, screen_mode, verbatim, json_schema, send_now, task_wake_fallback, tool_overrides_update, respond_to, persist_ack, parsed_prompt_tx)
                                .await;
                            if cancel_for_send_now {
                                session.cancel_turn_for_send_now(&mut replay_buffer).await;
                            }
                            SessionActor::maybe_start_running_task(session.clone(), completion_tx.clone()).await;
                        }
                        SessionCommand::SessionMode { session_mode, responds_to } => {
                            session.handle_session_mode(session_mode).await;
                            let _ = responds_to.send(());
                        }
                        SessionCommand::SetSessionModel { sampling_config, use_concise, apply_prompt_override, skip_prompt_rewrite, auto_compact_threshold_percent, responds_to } => {
                            let updated_model_id = session.handle_set_session_model(sampling_config, use_concise, apply_prompt_override, skip_prompt_rewrite, auto_compact_threshold_percent).await;
                            let _ = responds_to.send(updated_model_id);
                        }
                        SessionCommand::RebuildAgentForDefinition { definition, responds_to } => {
                            let outcome = session.handle_rebuild_agent_for_definition(definition).await;
                            let _ = responds_to.send(outcome);
                        }
                        SessionCommand::OverrideModelName { model_name, extra_headers, context_window } => {
                            // Update the actor's SamplingConfig model + headers + context window.
                            if let Some(mut cfg) = session.chat_state_handle.get_sampling_config().await {
                                tracing::info!(
                                    target: SESSION_LOG,
                                    session_id = %session.session_info.id,
                                    old_model = %cfg.model,
                                    new_model = %model_name,
                                    extra_header_count = extra_headers.len(),
                                    old_context_window = cfg.context_window.get(),
                                    new_context_window = ?context_window.map(|cw| cw.get()),
                                    "OVERRIDE_MODEL: changing model name in sampling config"
                                );
                                // Update signals so primaryModelId and modelsUsed
                                // reflect the model used after the override, not
                                // the agent-level default (e.g. "grok-4.5").
                                // set_primary_model also adds to models_used.
                                session.signals_handle().set_primary_model(&model_name);
                                cfg.model = model_name.clone();
                                cfg.extra_headers.extend(extra_headers);
                                if let Some(cw) = context_window
                                    && session.compaction.context_window_override.is_none()
                                {
                                    cfg.context_window = cw;
                                }
                                session.chat_state_handle.update_sampling_config(cfg);

                                let existing = session.chat_state_handle.get_credentials().await;
                                if let Some(r) = crate::agent::config::try_resolve_model_credentials(model_name.as_str(), existing.api_key.as_deref()) {
                                    session.chat_state_handle.update_credentials(xai_chat_state::Credentials {
                                        api_key: r.api_key,
                                        auth_type: r.auth_type,
                                        alpha_test_key: existing.alpha_test_key,
                                        client_version: existing.client_version,
                                    });
                                }
                                // Credentials changed under a possibly-unchanged model id.
                                session.invalidate_model_auth_memo();
                            }
                        }
                        SessionCommand::GetCurrentModel { responds_to } => {
                            let model = session.chat_state_handle.get_sampling_config().await
                                .map(|c| c.model)
                                .unwrap_or_default();
                            let _ = responds_to.send(model);
                        }
                        SessionCommand::GetCurrentPromptMode { responds_to } => {
                            let mode = *session.current_prompt_mode.lock();
                            let _ = responds_to.send(mode);
                        }
                        SessionCommand::GetModelMetadata { responds_to } => {
                            let id = session.chat_state_handle.get_last_model_metadata().await;
                            let _ = responds_to.send(id);
                        }
                        SessionCommand::GetSessionInfo { responds_to } => {
                            let info = session.build_session_info().await;
                            let _ = responds_to.send(info);
                        }
                        SessionCommand::BackgroundForegroundCommand { tool_call_id, respond_to } => {
                            let result = session.agent.borrow().tool_bridge()
                                .background_foreground_command(&tool_call_id)
                                .await;
                            let _ = respond_to.send(result);
                        }
                        SessionCommand::KillBackgroundTask { task_id, respond_to } => {
                            let result = session.agent.borrow().tool_bridge()
                                .kill_background_task(&task_id)
                                .await
                                .map_err(|e| e.to_string());
                            let _ = respond_to.send(result);
                        }
                        SessionCommand::DeleteScheduledTask { task_id, respond_to } => {
                            let result = session.agent.borrow().tool_bridge()
                                .delete_scheduled_task(&task_id)
                                .await
                                .map_err(|e| e.to_string());
                            let _ = respond_to.send(result);
                        }
                        SessionCommand::ListTasks { respond_to } => {
                            let result = session.agent.borrow().tool_bridge()
                                .list_tasks()
                                .await;
                            let _ = respond_to.send(result);
                        }
                        SessionCommand::GetHooksList { respond_to } => {
                            use crate::extensions::hooks::hook_spec_to_info;

                            let hooks = match &*session.hook_registry.borrow() {
                                Some(registry) => registry
                                    .all_hooks()
                                    .iter()
                                    .map(|spec| hook_spec_to_info(spec))
                                    .collect(),
                                None => Vec::new(),
                            };

                            // Report the folder-trust verdict so the flag matches
                            // the gated registry built above.
                            let project_trusted =
                                crate::agent::folder_trust::project_scope_allowed(
                                    std::path::Path::new(&session.session_info.cwd),
                                );

                            let _ = respond_to.send(xai_hooks_plugins_types::HooksListResponse {
                                hooks,
                                project_trusted,
                                load_errors: session.hook_load_errors.borrow().clone(),
                            });
                        }
                        SessionCommand::HooksAction { action, respond_to } => {
                            let outcome = session.handle_hooks_action(action).await;
                            let _ = respond_to.send(outcome);
                        }
                        SessionCommand::NotifyPluginUpdates { updates } => {
                            session
                                .send_xai_notification(
                                    XaiSessionUpdate::PluginUpdatesInstalled { updates },
                                )
                                .await;
                        }
                        SessionCommand::PluginsAction { action, respond_to } => {
                            let outcome = session.handle_plugins_action(action).await;
                            let _ = respond_to.send(outcome);
                        }
                        SessionCommand::PluginsList { respond_to } => {
                            let _ = respond_to.send(session.plugin_registry.borrow().clone());
                        }
                        SessionCommand::DispatchNotificationHook {
                            notification_type,
                            message,
                            title,
                            level,
                        } => {
                            session
                                .dispatch_notification_hook(
                                    &notification_type,
                                    message,
                                    title,
                                    level,
                                )
                                .await;
                        }
                        SessionCommand::DropMonitorNotifications { task_id } => {
                            // Discard pending + mid-turn-buffered monitor events
                            // for this task so a TaskCompleted auto-wake is the
                            // sole model-facing signal for natural exit.
                            {
                                let mut state = session.state.lock().await;
                                state.pending_notifications.retain(|n| {
                                    !matches!(
                                        &n.source,
                                        NotificationSource::MonitorEvent { task_id: tid }
                                            if tid == &task_id
                                    )
                                });
                            }
                            if let Some(buffer) = &session.tool_context.monitor_event_buffer {
                                let dropped = buffer.drain_matching(|e| e.task_id == task_id);
                                if !dropped.is_empty() {
                                    tracing::debug!(
                                        task_id = %task_id,
                                        dropped = dropped.len(),
                                        "dropped buffered monitor events after TaskCompleted auto-wake"
                                    );
                                }
                            }
                        }
                        SessionCommand::InjectNotification { prompt_id, prompt_blocks, priority, source } => {
                            let is_turn_active = session
                                .tool_context
                                .is_turn_active
                                .as_ref()
                                .map(|f| f.load(std::sync::atomic::Ordering::Relaxed))
                                .unwrap_or(false);

                            if is_turn_active && priority == NotificationPriority::Next {
                                // Mid-turn + Next: push to the shared buffer for
                                // the turn loop's `inject_pending_monitor_events`.
                                if let Some(buffer) = &session.tool_context.monitor_event_buffer {
                                    let non_text_count = prompt_blocks.iter().filter(|b| !matches!(b, acp::ContentBlock::Text(_))).count();
                                    if non_text_count > 0 {
                                        tracing::debug!(
                                            non_text_count,
                                            "Non-text content blocks dropped in mid-turn monitor event routing"
                                        );
                                    }

                                    let event_text = prompt_blocks
                                        .iter()
                                        .filter_map(|b| {
                                            if let acp::ContentBlock::Text(t) = b {
                                                Some(t.text.clone())
                                            } else {
                                                None
                                            }
                                        })
                                        .collect::<Vec<_>>()
                                        .join("\n");

                                    let task_id = source.task_id().to_owned();

                                    // Cap to prevent unbounded growth during long tool calls.
                                    const MAX_BUFFER_EVENTS: usize = 50;
                                    buffer.push_capped(
                                        xai_grok_tools::implementations::grok_build::task::types::MonitorEventNotification {
                                            task_id: task_id.clone(),
                                            event_text,
                                            // Tag with this session's id so the
                                            // shared (leader-mode) buffer drain
                                            // sites only surface it here. The
                                            // bridge guard guarantees this
                                            // event is owned by this session.
                                            owner_session_id: Some(
                                                session.session_info.id.0.to_string(),
                                            ),
                                        },
                                        MAX_BUFFER_EVENTS,
                                    );

                                    tracing::debug!(
                                        task_id = %task_id,
                                        "Routed monitor event to mid-turn buffer"
                                    );
                                }
                            } else {
                                {
                                    let mut state = session.state.lock().await;
                                    SessionActor::push_pending_notification(
                                        &mut state,
                                        PendingNotification {
                                            prompt_id,
                                            prompt_blocks,
                                            priority,
                                            source,
                                        },
                                    );
                                }
                                SessionActor::maybe_drain_notifications(session.clone(), completion_tx.clone()).await;
                            }
                        }
                        SessionCommand::RecordGoalTurnTaskIds { task_ids } => {
                            session.record_reparented_goal_turn_task_ids(task_ids);
                        }
                        SessionCommand::RemoveQueuedPrompt { id, expected_version, owner } => {
                            session.handle_remove_queued_prompt(&id, expected_version, owner.as_deref()).await;
                        }
                        SessionCommand::ReorderQueue { ordered_ids } => {
                            session.handle_reorder_queue(&ordered_ids).await;
                        }
                        SessionCommand::ClearQueue { owner } => {
                            session.handle_clear_queue(owner.as_deref()).await;
                        }
                        SessionCommand::EditQueuedPrompt { id, new_text, editor } => {
                            session.handle_edit_queued_prompt(&id, new_text, editor.as_deref()).await;
                        }
                        SessionCommand::HoldCombineEdit { id } => {
                            let mut state = session.state.lock().await;
                            state.combine_edit_holds.insert(id);
                        }
                        SessionCommand::ReleaseCombineEdit { id } => {
                            let mut state = session.state.lock().await;
                            state.combine_edit_holds.remove(&id);
                        }
                        SessionCommand::InterjectQueuedPrompt { id, expected_version, owner, new_text } => {
                            // Send-now: the handler promoted the row; cancel the running turn and start it.
                            let cancel_for_send_now = session.handle_interject_queued_prompt(&id, expected_version, owner.as_deref(), new_text.as_deref()).await;
                            if cancel_for_send_now {
                                session.cancel_turn_for_send_now(&mut replay_buffer).await;
                            }
                            SessionActor::maybe_start_running_task(session.clone(), completion_tx.clone()).await;
                        }
                        SessionCommand::Cancel {
                            cancel_subagents,
                            kill_background_tasks,
                            rewind_if_pristine,
                            trigger,
                        } => {
                            // Flush the actor-owned replay buffer before tearing
                            // down the running turn so any streamed chunks
                            // (notably AgentThoughtChunk reasoning text) still
                            // pending at cancel time are committed to
                            // updates.jsonl. Without this, the tail of a long
                            // reasoning stream sitting in the buffer when the
                            // user hits Ctrl+C never reaches disk before the
                            // trace upload snapshots the session directory.
                            // Mirrors the pattern in `FlushComplete` below.
                            if let Some(notification) = replay_buffer.flush() {
                                session.emit_buffered(notification).await;
                            }
                            // Clear pending interjections — the turn is being
                            // cancelled, so they have no active turn to inject into.
                            session.pending_interjections.clear();
                            let suppress_task_wakes = trigger.as_deref() == Some("ctrl_c");
                            session
                                .cancel_running_task(
                                    cancel_subagents,
                                    kill_background_tasks,
                                    rewind_if_pristine,
                                    trigger,
                                )
                                .await;

                            // Auto-pause active goal on Ctrl+C so timers stop
                            // and the pager shows "paused" instead of "active".
                            // Shared with the doom-loop and back-off paths via
                            // `auto_pause_goal_if_active`.
                            session
                                .auto_pause_goal_if_active(
                                    crate::session::goal_tracker::GoalPauseReason::User,
                                )
                                .await;

                            // Kick any already-queued prompt so it doesn't sit
                            // waiting for a completion message that will never
                            // arrive (the aborted task can't send one).
                            SessionActor::maybe_start_running_task(session.clone(), completion_tx.clone()).await;
                            // Ctrl+C leaves pending notifications suppressed. Other
                            // cancel triggers leave the actor eligible for its normal idle drain.
                            if !suppress_task_wakes {
                                SessionActor::maybe_drain_notifications(
                                    session.clone(),
                                    completion_tx.clone(),
                                )
                                .await;
                            }
                        }
                        SessionCommand::CompactSession { user_context, respond_to } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                let compact_session = s.run_compact(user_context).await;
                                let _ = respond_to.send(compact_session);
                            });
                        }
                        SessionCommand::ReloadPlugins { registry } => {
                            // Eager fan-out: a plugin was added/removed/reloaded
                            // in another session. Adopt the pushed snapshot so this
                            // session's hooks, MCP, skills, and the client's
                            // slash-command catalog match — the same refresh the
                            // originating session gets, so switching here needs no
                            // lazy refetch. Subagents inherit the parent registry.
                            if !session.startup_hints.is_subagent {
                                // Fan-outs rebuild without per-session `_meta.pluginDirs`;
                                // re-merge this session's own dirs before adopting.
                                let registry = session.preserve_session_plugin_dirs(registry);
                                session.apply_plugin_registry_snapshot(registry).await;
                            }
                        }
                        SessionCommand::ReloadHooks => {
                            // Re-discover the session's project hooks on the
                            // now-flipped folder-trust verdict (e.g. after an
                            // interactive trust grant). Reuses the same path as
                            // `/hooks reload`; subagents inherit via the parent.
                            // Run INLINE on the serialized command loop (not a
                            // spawned task) like `ReloadPlugins`: `reload_hooks_impl`
                            // mutates `hook_registry`, and this actor's safety
                            // invariant (file-header `await_holding_refcell_ref`
                            // allow) is "no concurrent mutation" of it — spawning
                            // would race turn tasks.
                            if !session.startup_hints.is_subagent {
                                let _ = session.reload_hooks_impl().await;
                            }
                        }
                        SessionCommand::RefreshSkillBaseline => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                let cwd = s.tool_context.cwd.as_path().to_string_lossy();
                                let skills_config = crate::util::config::load_config().await.skills;
                                let pr = s.plugin_registry.borrow().clone();
                                let new_skills = xai_grok_agent::prompt::skills::list_skills_with_plugins(
                                    Some(&cwd),
                                    &skills_config,
                                    pr.as_deref(),
                                    s.rebuild_spec.compat,
                                )
                                .await;
                                tracing::info!(skills = new_skills.len(), "refreshed skill baseline after bundle sync");
                                let bridge = s.agent.borrow().tool_bridge().clone();
                                bridge.update_skill_baseline(new_skills).await;
                                if let Some(effects) = bridge.apply_pending_skill_update().await {
                                    s.apply_skill_update_effects(effects).await;
                                }
                            });
                        }
                        SessionCommand::FlushMemory { respond_to } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                if s.memory.is_enabled() {
                                    let did_flush = s.run_memory_flush("user_requested", None).await;
                                    let _ = respond_to.send(Ok(did_flush));
                                } else {
                                    let _ = respond_to.send(Err(
                                        acp::Error::invalid_request()
                                            .data("memory is not enabled for this session".to_string())
                                    ));
                                }
                            });
                        }
                        SessionCommand::SetYoloMode { enabled } => {
                            let was = session.permissions.is_yolo_mode();
                            tracing::info!("Session received SetYoloMode: {}", enabled);
                            session.permissions.set_yolo_mode(enabled);
                            // Report the ACTUAL state, not the request: the manager
                            // clamps a requested ON to OFF under the always-approve
                            // pin, so emitting `enabled` would announce a turn-on
                            // that never happened.
                            let actual = session.permissions.is_yolo_mode();
                            if let Some(enabled) = yolo_toggle_report(was, actual) {
                                session.emit_event(crate::session::events::Event::YoloToggled { enabled });
                            }
                        }
                        SessionCommand::SetAutoMode { enabled } => {
                            // Feature gate: a runtime request to enable auto is
                            // honored only when the feature is enabled, so a
                            // client notification can't bypass the gate.
                            let enabled = enabled
                                && crate::util::config::auto_permission_mode_enabled_from_disk();
                            tracing::info!("Session received SetAutoMode: {}", enabled);
                            session.permissions.set_auto_mode(enabled);
                            if enabled {
                                session.wire_permission_auto_llm_classifier().await;
                            } else {
                                session.permissions.set_llm_side_query_wired(false);
                            }
                        }
                        SessionCommand::ResetPermissionState => {
                            session.permissions.reset_state();
                            tracing::info!(
                                session_id = %session.session_info.id,
                                "Permission state reset via notification"
                            );
                        }
                        SessionCommand::Rewind { request, respond_to } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                let result = s.handle_rewind(request).await;
                                let _ = respond_to.send(result);
                            });
                        }
                        SessionCommand::RepairHistory { dry_run, respond_to } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                let result = s.handle_repair_history(dry_run).await;
                                let _ = respond_to.send(result);
                            });
                        }
                        SessionCommand::GetRewindPoints { respond_to } => {
                            let response = session.get_rewind_points().await;
                            let _ = respond_to.send(response);
                        }
                        SessionCommand::GetRewindFileCounts { respond_to } => {
                            let _ = respond_to.send(session.rewind_file_counts().await);
                        }
                        SessionCommand::ReconcileRewindTracker { target_prompt_index } => {
                            session.merge_rewind_tracker_from(target_prompt_index).await;
                        }
                        SessionCommand::XaiSessionNotification { notification } => {
                            session.handle_xai_session_notification(notification).await;
                        }
                        SessionCommand::RecordSubagentUsage {
                            by_model,
                            parent_prompt_id,
                            incomplete,
                            respond_to,
                        } => {
                            use super::updates::SubagentUsageApply;
                            match session
                                .record_subagent_usage(
                                    &by_model,
                                    parent_prompt_id.as_deref(),
                                    incomplete,
                                )
                                .await
                            {
                                Ok(SubagentUsageApply::AttributedToPrompt) => {
                                    // Any nested incomplete is already on the ledger;
                                    // no sticky mark needed.
                                    let _ = respond_to.send(());
                                }
                                Ok(SubagentUsageApply::SessionOnly) => {
                                    // Report-level sticky: the stamped prompt's bill
                                    // under-counts.
                                    let _ = session
                                        .mark_subagent_usage_not_applied(
                                            parent_prompt_id.as_deref(),
                                        )
                                        .await;
                                    let _ = respond_to.send(());
                                }
                                // Drop oneshot → fold_acked=false on child; true-miss path runs.
                                Err(()) => {}
                            }
                        }
                        SessionCommand::MarkSubagentUsageNotApplied {
                            parent_prompt_id,
                            respond_to,
                        } => {
                            // True apply-miss: sticky + pin-aware ledger fail-closed.
                            if session
                                .mark_apply_miss_incomplete(parent_prompt_id.as_deref())
                                .await
                            {
                                let _ = respond_to.send(());
                            }
                        }
                        SessionCommand::ErrorPathUsageFallback {
                            prompt_id,
                            respond_to,
                        } => {
                            let pid = prompt_id.or_else(|| {
                                session
                                    .current_prompt_id
                                    .lock()
                                    .ok()
                                    .and_then(|g| g.clone())
                            });
                            let usage = match pid.as_deref() {
                                Some(id) => session.error_path_usage_fallback(id).await,
                                None => {
                                    match session.chat_state_handle.try_get_prompt_usage().await {
                                        Ok(ledger) => {
                                            crate::extensions::notification::PromptUsage::for_error_path(
                                                ledger.as_ref(),
                                                false,
                                            )
                                        }
                                        Err(()) => {
                                            crate::extensions::notification::PromptUsage::for_error_path(
                                                None, true,
                                            )
                                        }
                                    }
                                }
                            };
                            let _ = respond_to.send(usage);
                        }
                        SessionCommand::SetNextTraceTurn {
                            next_trace_turn,
                            request_id,
                        } => {
                            let _ =
                                session.notifications.persistence_tx.send(PersistenceMsg::NextTraceTurn {
                                    next_trace_turn,
                                    request_id,
                                });
                        }
                        SessionCommand::CopyFile { respond_to } => {
                            // Flush the actor-owned replay buffer first so any
                            // buffered notifications (e.g. streamed reasoning
                            // chunks emitted during sampler teardown after a
                            // cancel) are committed to updates.jsonl before the
                            // persistence task snapshots the session directory.
                            // `PersistenceMsg` is FIFO on `persistence_tx`, so
                            // the `Update` produced by `emit_buffered` lands
                            // before `CopyFile`, and `flush_and_sync` on the
                            // persistence side then sees it on disk.
                            if let Some(notification) = replay_buffer.flush() {
                                session.emit_buffered(notification).await;
                            }
                            let _ = session
                                .notifications.persistence_tx
                                .send(PersistenceMsg::CopyFile { one_shot: respond_to });
                        }
                        SessionCommand::IsBusy { respond_to } => {
                            // "Any work pending?" — a running turn or queued
                            // inputs. Consulted by the leader's idle-unload
                            // decision. Cheap: a single state lock.
                            let busy = {
                                let state = session.state.lock().await;
                                state_is_busy(&state)
                            };
                            let _ = respond_to.send(busy);
                        }
                        SessionCommand::FlushComplete { respond_to } => {
                            // Flush the actor-owned replay buffer inline. This branch
                            // already runs inside `run_session()`, so sending a replay
                            // flush event to `event_tx` would deadlock waiting for the
                            // same loop to process its own mailbox.
                            if let Some(notification) = replay_buffer.flush() {
                                session.emit_buffered(notification).await;
                            }
                            // Chain through persistence actor — only signal after
                            // flush_pending() completes on disk. This makes
                            // FlushComplete a true sync barrier (unlike the old
                            // pattern which signaled before the persistence actor
                            // processed the flush).
                            let _ = session
                                .notifications.persistence_tx
                                .send(PersistenceMsg::FlushAndAck { respond_to });
                        }
                        SessionCommand::UpdateMcpServers { mcp_servers, respond_to } => {
                            if session.startup_hints.is_subagent {
                                tracing::debug!(
                                    session_id = %session.session_info.id.0,
                                    "Skipping UpdateMcpServers for subagent session",
                                );
                                let _ = respond_to.send(Ok(()));
                                continue;
                            }
                            tracing::info!(
                                "Updating MCP servers for session '{}' ({} servers)",
                                session.session_info.id.0,
                                mcp_servers.len()
                            );

                            // Re-seed the session-scoped MCP output cap
                            // (repo `[mcp] max_output_bytes`) BEFORE the
                            // unchanged-diff early-exit below: this command
                            // also fires for `<cwd>/.grok/config.toml` edits,
                            // and a cap-only edit changes no server configs.
                            session.reseed_mcp_output_cap().await;

                            // Capture the dispatcher's
                            // event sender alongside the diff so we
                            // can fan out `McpClientEvent::ConfigDiff`
                            // immediately after the in-memory swap
                            // completes — without holding the
                            // `mcp_state` lock across the emit.
                            let (diff, dispatch_event_tx) = {
                                let mut mcp_state = session.mcp_state.lock().await;
                                let diff = mcp_state.update_configs_diff(mcp_servers);
                                let tx = mcp_state.client_event_tx();
                                (diff, tx)
                            };

                            let Some(diff) = diff else {
                                tracing::debug!(
                                    "MCP configs unchanged for session '{}', skipping re-initialization",
                                    session.session_info.id.0
                                );
                                let _ = respond_to.send(Ok(()));
                                continue;
                            };

                            // Emit one `ConfigDiff` so the
                            // `StatusDispatcher` fans out per-server
                            // `mcp/server_status` with
                            // `reason: ConfigAdded` / `ConfigRemoved`.
                            // Best-effort — a dropped dispatcher
                            // means `mcp.liveness_watchers` is
                            // off or the session has shut down; the
                            // tool-bridge tear-down and re-init below
                            // still happen.
                            if (!diff.added.is_empty() || !diff.removed.is_empty())
                                && let Some(tx) = &dispatch_event_tx
                            {
                                let _ = tx.send(
                                    xai_grok_mcp::servers::McpClientEvent::ConfigDiff {
                                        added: diff.added.clone(),
                                        removed: diff.removed.clone(),
                                    },
                                );
                            }

                            for name in &diff.removed {
                                let prefix = format!(
                                    "{}{}",
                                    name,
                                    crate::session::mcp_servers::MCP_TOOL_NAME_DELIMITER
                                );
                                let removed_count = session
                                    .agent
                                    .borrow()
                                    .tool_bridge()
                                    .unregister_tools_by_prefix(&prefix);
                                tracing::info!(
                                    server = name.as_str(),
                                    tools_removed = removed_count,
                                    "Unregistered tools for removed MCP server"
                                );
                            }

                            let session_for_mcp = session.clone();
                            tokio::task::spawn_local(async move {
                                session_for_mcp.ensure_mcp_tools_initialized().await;
                                let _ = respond_to.send(Ok(()));
                            });
                        }
                        SessionCommand::ToggleMcpServer { server_name, enabled, server_config, respond_to } => {
                            session.events.emit(xai_file_utils::events::Event::McpServerToggled {
                                server_name: server_name.clone(),
                                enabled,
                            });
                            let mut mcp_state = session.mcp_state.lock().await;
                            let mut configs = mcp_state.configs.clone();

                            if enabled {
                                if let Some(config) = server_config {
                                    // Replace any prior entry so setup → enable can
                                    // swap an unresolved placeholder for a resolved URL.
                                    configs.retain(|c| {
                                        crate::session::mcp_servers::mcp_server_name(c)
                                            != server_name
                                    });
                                    configs.push(config);
                                } else {
                                    let already_present = configs.iter().any(|c| {
                                        crate::session::mcp_servers::mcp_server_name(c)
                                            == server_name
                                    });
                                    if already_present {
                                        drop(mcp_state);
                                        let _ = respond_to.send(Ok(()));
                                        continue;
                                    }
                                    drop(mcp_state);
                                    let _ = respond_to.send(Err(acp::Error::invalid_params()
                                        .data(format!("server '{}' not found in config", server_name))));
                                    continue;
                                }
                            } else {
                                configs.retain(|c| crate::session::mcp_servers::mcp_server_name(c) != server_name);
                            }

                            let diff = mcp_state.update_configs_diff(configs);
                            // Snapshot the dispatcher
                            // sender BEFORE dropping the lock so the
                            // emit below survives any later mutation.
                            let dispatch_event_tx = mcp_state.client_event_tx();
                            drop(mcp_state);

                            let Some(diff) = diff else {
                                let _ = respond_to.send(Ok(()));
                                continue;
                            };

                            // ToggleMcpServer mirrors
                            // UpdateMcpServers — fan out per-server
                            // status via the dispatcher (`ConfigAdded`
                            // / `ConfigRemoved` reason codes on
                            // `mcp/server_status`).
                            if (!diff.added.is_empty() || !diff.removed.is_empty())
                                && let Some(tx) = &dispatch_event_tx
                            {
                                let _ = tx.send(
                                    xai_grok_mcp::servers::McpClientEvent::ConfigDiff {
                                        added: diff.added.clone(),
                                        removed: diff.removed.clone(),
                                    },
                                );
                            }

                            for name in &diff.removed {
                                let prefix = format!(
                                    "{}{}",
                                    name,
                                    crate::session::mcp_servers::MCP_TOOL_NAME_DELIMITER
                                );
                                let removed_count = session
                                    .agent
                                    .borrow()
                                    .tool_bridge()
                                    .unregister_tools_by_prefix(&prefix);
                                tracing::info!(
                                    server = name.as_str(),
                                    tools_removed = removed_count,
                                    "Unregistered tools for toggled MCP server"
                                );
                            }

                            let session_for_mcp = session.clone();
                            let sname = server_name.clone();
                            tokio::task::spawn_local(async move {
                                session_for_mcp.ensure_mcp_tools_initialized().await;
                                if let Err(e) = crate::util::config::save_mcp_server_enabled(
                                    &sname, enabled,
                                ).await {
                                    tracing::warn!(
                                        server = sname.as_str(),
                                        error = %e,
                                        "Failed to persist server enabled state to config"
                                    );
                                }
                                let _ = respond_to.send(Ok(()));
                            });
                        }
                        SessionCommand::ToggleMcpTool { server_name, tool_name, enabled, is_managed_gateway, respond_to } => {
                            if is_managed_gateway {
                                let mut disabled_tools = crate::util::config::get_all_mcp_disabled_tools(std::path::Path::new(&session.session_info.cwd));
                                if tool_name.is_empty() {
                                    let set = disabled_tools
                                        .entry(crate::util::config::MANAGED_GATEWAY_DISABLED_CONNECTORS_KEY.to_string())
                                        .or_default();
                                    if enabled {
                                        set.remove(&server_name);
                                    } else {
                                        set.insert(server_name.clone());
                                    }
                                    if set.is_empty() {
                                        disabled_tools.remove(crate::util::config::MANAGED_GATEWAY_DISABLED_CONNECTORS_KEY);
                                    }
                                } else if enabled {
                                    if let Some(set) = disabled_tools.get_mut(&server_name) {
                                        set.remove(&tool_name);
                                        if set.is_empty() {
                                            disabled_tools.remove(&server_name);
                                        }
                                    }
                                } else {
                                    disabled_tools
                                        .entry(server_name.clone())
                                        .or_default()
                                        .insert(tool_name.clone());
                                }

                                session
                                    .refresh_mcp_snapshot_and_schedule_reminder_with_disabled(
                                        &disabled_tools,
                                    )
                                    .await;
                                session.refresh_goal_harness_enabled().await;

                                let disabled_vec: Vec<String> = if tool_name.is_empty() {
                                    disabled_tools
                                        .get(crate::util::config::MANAGED_GATEWAY_DISABLED_CONNECTORS_KEY)
                                        .map(|s| s.iter().cloned().collect())
                                        .unwrap_or_default()
                                } else {
                                    disabled_tools
                                        .get(&server_name)
                                        .map(|s| s.iter().cloned().collect())
                                        .unwrap_or_default()
                                };
                                let notifications = session.notifications.gateway.clone();
                                let session_id = session.session_info.id.0.clone();
                                let server_for_persist = if tool_name.is_empty() {
                                    crate::util::config::MANAGED_GATEWAY_DISABLED_CONNECTORS_KEY.to_string()
                                } else {
                                    server_name.clone()
                                };
                                tokio::task::spawn_local(async move {
                                    if let Err(e) = crate::util::config::save_mcp_disabled_tools(
                                        &server_for_persist,
                                        &disabled_vec,
                                    ).await {
                                        tracing::warn!(
                                            server = server_for_persist.as_str(),
                                            error = %e,
                                            "Failed to persist disabled_tools to config"
                                        );
                                    }
                                    let payload = crate::extensions::mcp::McpToolsChanged {
                                        session_id: session_id.to_string(),
                                        server_name: String::new(),
                                        tools: Vec::new(),
                                    };
                                    if let Ok(params) = serde_json::value::to_raw_value(&payload) {
                                        notifications.forward_fire_and_forget(acp::ExtNotification::new("x.ai/mcp/tools_changed", params.into()));
                                    }
                                    let _ = respond_to.send(Ok(()));
                                });
                                continue;
                            }
                            let qualified = format!(
                                "{}{}{}",
                                server_name,
                                crate::session::mcp_servers::MCP_TOOL_NAME_DELIMITER,
                                tool_name,
                            );
                            let mut mcp_state = session.mcp_state.lock().await;

                            if enabled {
                                // Re-enable: remove from disabled set, re-register from stashed registration.
                                if let Some(set) = mcp_state.disabled_tools.get_mut(&server_name) {
                                    set.remove(&tool_name);
                                    if set.is_empty() {
                                        mcp_state.disabled_tools.remove(&server_name);
                                    }
                                }
                                if let Some(reg) = mcp_state.disabled_tool_registrations.remove(&qualified)
                                    && reg.model_visible
                                {
                                    let bridge = session.agent.borrow().tool_bridge().clone();
                                    if let Err(e) = bridge
                                        .register_mcp_tools(reg.name, reg.tool, Some(reg.input_schema))
                                        .await
                                    {
                                        tracing::warn!(
                                            tool = qualified.as_str(),
                                            error = %e,
                                            "Failed to re-register toggled MCP tool"
                                        );
                                    }
                                }
                            } else {
                                // Disable: stash a registration so the tool can be
                                // re-enabled without a full re-init, then unregister.
                                let bridge = session.agent.borrow().tool_bridge().clone();
                                let tool_def = bridge
                                    .tool_definitions()
                                    .await
                                    .into_iter()
                                    .find(|d| d.function.name == qualified);
                                if let Some(def) = tool_def {
                                    let meta = mcp_state.mcp_tool_meta.get(&qualified).cloned();
                                    let schema = def.function.parameters.clone();
                                    let mcp_tool = crate::session::mcp_servers::McpTool::new(
                                        tool_name.clone(),
                                        def.function.description.clone().unwrap_or_default(),
                                        server_name.clone(),
                                        session.mcp_state.clone(),
                                        schema,
                                        meta,
                                    );
                                    if let Some(reg) = mcp_tool.into_registration() {
                                        mcp_state
                                            .disabled_tool_registrations
                                            .insert(qualified.clone(), reg);
                                    }
                                }
                                bridge.unregister_tool_by_name(&qualified);
                                mcp_state
                                    .disabled_tools
                                    .entry(server_name.clone())
                                    .or_default()
                                    .insert(tool_name.clone());
                            }

                            // Collect the new disabled set for this server before dropping lock.
                            let disabled_vec: Vec<String> = mcp_state
                                .disabled_tools
                                .get(&server_name)
                                .map(|s| s.iter().cloned().collect())
                                .unwrap_or_default();
                            drop(mcp_state);

                            session.refresh_mcp_snapshot_and_schedule_reminder().await;
                            session.refresh_goal_harness_enabled().await;

                            // Persist to config and emit notification in background.
                            let notifications = session.notifications.gateway.clone();
                            let session_id = session.session_info.id.0.clone();
                            let server_for_persist = server_name.clone();
                            tokio::task::spawn_local(async move {
                                if let Err(e) = crate::util::config::save_mcp_disabled_tools(
                                    &server_for_persist,
                                    &disabled_vec,
                                ).await {
                                    tracing::warn!(
                                        server = server_for_persist.as_str(),
                                        error = %e,
                                        "Failed to persist disabled_tools to config"
                                    );
                                }
                                // Emit the
                                // typed McpToolsChanged shape with
                                // `sessionId` populated so the pager
                                // can route via `find_session_match`.
                                // The toggle-tool path is not
                                // server-scoped (the disable mask
                                // applies to one server but the
                                // pager refetches the full catalog),
                                // so `server_name` / `tools` stay
                                // empty and skip-if-empty drops them
                                // from the wire — identical bytes to
                                // the previous payload save for the
                                // additional `sessionId` field.
                                let payload = crate::extensions::mcp::McpToolsChanged {
                                    session_id: session_id.to_string(),
                                    server_name: String::new(),
                                    tools: Vec::new(),
                                };
                                if let Ok(params) =
                                    serde_json::value::to_raw_value(&payload)
                                {
                                    notifications.forward_fire_and_forget(acp::ExtNotification::new(crate::extensions::mcp::mcp_methods::TOOLS_CHANGED
                                            , params.into()));
                                }
                                let _ = respond_to.send(Ok(()));
                            });
                        }
                        SessionCommand::SnapshotMcpPool { respond_to } => {
                            let mcp_state = session.mcp_state.lock().await;
                            let pool = if mcp_state.owned_clients.is_empty() && mcp_state.shared_clients.is_empty() {
                                None
                            } else {
                                Some(crate::session::mcp_servers::SharedMcpPool::from_state(&mcp_state))
                            };
                            let _ = respond_to.send(pool);
                        }
                        SessionCommand::SnapshotClientHooks { respond_to } => {
                            let _ = respond_to.send(session.client_hooks.borrow().clone());
                        }
                        SessionCommand::SnapshotToolDefinitions { respond_to } => {
                            // Use the SAME helper the turn uses so the snapshot can
                            // never drift from the parent turn's tool list. Excludes
                            // the structured-output tool (the turn appends that later).
                            let defs = session.prepare_tool_definitions_inner().await;
                            let specs = session.turn_base_tool_specs(&defs);
                            let _ = respond_to.send(specs);
                        }
                        SessionCommand::SetClientHooks { hooks } => {
                            *session.client_hooks.borrow_mut() = hooks;
                        }
                        SessionCommand::GetMcpStatus { respond_to } => {
                            let mcp_state = session.mcp_state.clone();
                            let tool_bridge = session.agent.borrow().tool_bridge().clone();
                            let writer = session.events.writer();
                            tokio::task::spawn_local(async move {
                                let snapshot = crate::extensions::mcp::build_mcp_status(
                                    &mcp_state,
                                    &tool_bridge,
                                    Some(&writer),
                                ).await;
                                let _ = respond_to.send(snapshot);
                            });
                        }
                        SessionCommand::CallMcpTool { server_name, server_url, tool_name, arguments, respond_to } => {
                            let mcp_state = session.mcp_state.clone();
                            tokio::task::spawn_local(async move {
                                let result = crate::extensions::mcp::call_mcp_tool(
                                    &mcp_state,
                                    &server_name,
                                    server_url.as_deref(),
                                    &tool_name,
                                    arguments,
                                ).await;
                                let _ = respond_to.send(result);
                            });
                        }
                        SessionCommand::ReadMcpResource { server_name, uri, respond_to } => {
                            let mcp_state = session.mcp_state.clone();
                            tokio::task::spawn_local(async move {
                                let result = crate::extensions::mcp::read_mcp_resource(
                                    &mcp_state,
                                    &server_name,
                                    &uri,
                                ).await;
                                let _ = respond_to.send(result);
                            });
                        }
                        SessionCommand::McpAuthStatus { respond_to } => {
                            let mcp_state = session.mcp_state.clone();
                            tokio::task::spawn_local(async move {
                                let state = mcp_state.lock().await;
                                let entries: Vec<_> = state.auth_required.iter().map(|name| {
                                    crate::extensions::mcp::McpAuthStatusEntry {
                                        server_name: name.clone(),
                                        status: "needs_auth",
                                    }
                                }).collect();
                                let _ = respond_to.send(entries);
                            });
                        }
                        SessionCommand::McpAuthTrigger { server_name, respond_to } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                let result = s.handle_mcp_auth_trigger(&server_name).await;
                                let _ = respond_to.send(result);
                            });
                        }
                        SessionCommand::GetManagedGatewayDisabledTools { respond_to } => {
                            let disabled_tools = crate::util::config::get_all_mcp_disabled_tools(
                                std::path::Path::new(&session.session_info.cwd),
                            );
                            let _ = respond_to.send(disabled_tools);
                        }
                        SessionCommand::RetryAuthRequiredServers { respond_to } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                s.retry_auth_required_servers().await;
                                let _ = respond_to.send(());
                            });
                        }
                        SessionCommand::RefreshMcpSearchIndex => {
                            session.refresh_mcp_snapshot_and_schedule_reminder().await;
                        }
                        SessionCommand::TriggerTestFeedback { tier, mode, respond_to } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                let request = s.feedback_manager.force_feedback_request(tier, mode).await;
                                let notification = crate::extensions::notification::FeedbackRequestNotification::from(request.clone());
                                s.send_feedback_notification(request).await;
                                let resp = ExtMethodResult::success(notification).to_ext_response();
                                let _ = respond_to.send(resp);
                            });
                        }
                        SessionCommand::PersistFeedback(entry) => {
                            let _ = session
                                .notifications.persistence_tx
                                .send(PersistenceMsg::Feedback(*entry));
                        }
                        SessionCommand::AdvertiseCommands => {
                            session.send_available_commands_update().await;
                        }
                        SessionCommand::GetWorkflowCatalogState { respond_to } => {
                            let tool_names = session.registered_tool_names().await;
                            let has_runs = !session.workflow_tracker().await.lock().list().is_empty();
                            let availability =
                                session.build_command_availability(&tool_names, has_runs);
                            let _ = respond_to
                                .send((availability.workflows, availability.workflow_management));
                        }
                        SessionCommand::ListAvailableCommands { respond_to } => {
                            let bridge = session.agent.borrow().tool_bridge().clone();
                            let skills = bridge.slash_skills().await;
                            let tool_names = session.registered_tool_names().await;
                            let has_runs = !session.workflow_tracker().await.lock().list().is_empty();
                            let availability =
                                session.build_command_availability(&tool_names, has_runs);
                            let (_, workflows) = session.named_workflow_snapshot();
                            let commands = slash_commands::available_commands(
                                &skills,
                                availability,
                                &workflows,
                            );
                            let _ = respond_to.send(slash_commands::ListCommandsResponse {
                                commands,
                                tools: Some(tool_names),
                            });
                        }
                        SessionCommand::ReloadSkills => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                s.reload_skills_from_disk().await;
                            });
                        }
                        SessionCommand::DispatchSessionStartHook { source } => {
                            let envelope = session.fire_hook(
                                xai_grok_hooks::event::HookEventName::SessionStart,
                                None,
                                xai_grok_hooks::event::HookPayload::SessionStart {
                                    source,
                                    model_id: None,
                                    agent_type: None,
                                },
                            );
                            if let Some(registry) = session.hook_registry.borrow().clone() {
                                let ctx = session.hook_run_ctx();
                                let results = xai_grok_hooks::dispatcher::dispatch_non_blocking(
                                    &registry,
                                    xai_grok_hooks::event::HookEventName::SessionStart,
                                    &envelope,
                                    &ctx,
                                )
                                .await;
                                session.send_hook_execution("session_start", None, None, &results).await;
                            }
                        }
                        SessionCommand::GetFeedbackContext { turn_number, responds_to } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                use prod_mc_cli_chat_proxy_types::feedback_types::FeedbackToolOutcome;

                                // When the client provided a turn_number (per-turn
                                // feedback on a specific assistant message in the
                                // chat history), look up THAT turn's user/assistant
                                // text.
                                let turn_idx =
                                    turn_number.and_then(|n| usize::try_from(n).ok());
                                let (last_user_message, last_assistant_message) = match turn_idx {
                                    Some(n) => {
                                        let conv = s.chat_state_handle.get_conversation().await;
                                        turn_texts_for_feedback(&conv, n)
                                    }
                                    None => {
                                        tokio::join!(
                                            s.chat_state_handle.get_last_user_query_text(),
                                            s.chat_state_handle.get_last_assistant_text(),
                                        )
                                    }
                                };

                                let sh = s.signals_handle();
                                let (signals, tool_outcomes) = tokio::join!(
                                    sh.snapshot(),
                                    sh.last_turn_tool_outcomes(),
                                );
                                let signals = signals.unwrap_or_default();

                                let ctx = FeedbackContext {
                                    last_user_message,
                                    last_assistant_message,
                                    tool_outcomes: tool_outcomes
                                        .into_iter()
                                        .map(|o| FeedbackToolOutcome {
                                            tool_name: o.tool_name,
                                            calls: o.successes + o.failures,
                                            failures: o.failures,
                                        })
                                        .collect(),
                                    compaction_count: signals.compaction_count as i64,
                                    context_window_usage: signals.context_window_usage,
                                    context_tokens_used: signals.context_tokens_used,
                                    context_window_tokens: signals.context_window_tokens,
                                    session_cwd: s.tool_context.cwd.as_path().to_string_lossy().to_string(),
                                };
                                let _ = responds_to.send(ctx);
                            });
                        }
                        SessionCommand::GetActiveAgent { responds_to } => {
                            let agent_type = session.active_agent_type.lock().clone();
                            let _ = responds_to.send(agent_type);
                        }
                        SessionCommand::SideQuestion { question, respond_to } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                let result = s.handle_side_question(&question).await;
                                let _ = respond_to.send(result);
                            });
                        }
                        SessionCommand::Recap { auto } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                s.handle_recap(auto).await;
                            });
                        }
                        SessionCommand::AISuggest { prefix, cwd, model_override, respond_to } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                let result = s.handle_ai_suggest(&prefix, &cwd, model_override.as_deref()).await;
                                let _ = respond_to.send(result);
                            });
                        }
                        SessionCommand::SuggestPrompt { model_override, respond_to } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                let result = s.handle_suggest_prompt(model_override.as_deref()).await;
                                let _ = respond_to.send(result);
                            });
                        }
                        SessionCommand::RewriteMemoryNote { raw_text, context_summary, respond_to } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                let result = s.handle_rewrite_memory_note(&raw_text, &context_summary).await;
                                let _ = respond_to.send(result);
                            });
                        }
                        SessionCommand::Interject { text, id, images } => {
                            // Broadcast to every attached client so all panes
                            // viewing this session render the interjection block
                            // — not just the originating client. The originator
                            // dedups this echo by `id` against its optimistic
                            // local block; viewers render it.
                            session.broadcast_interjection(&text, id.as_deref());
                            // Telemetry at enqueue (not drain) so it is recorded
                            // even when a cancel clears the buffer before the
                            // next drain point.
                            session.events.emit(crate::session::events::Event::Interjected {
                                source: crate::session::events::InterjectionSource::Direct,
                                image_count: images.len() as u32,
                                redirect_kind: crate::session::events::RedirectKind::Interjection,
                            });
                            // Buffer only into an actually-running turn — the
                            // buffer is drained exclusively by the turn loop, so
                            // an interjection arriving while idle (the pager's
                            // running-state check races turn end) would strand
                            // forever and silently drop the user's message. Run
                            // it as its own prompt turn instead.
                            let turn_running = session
                                .current_prompt_id
                                .lock()
                                .ok()
                                .and_then(|g| g.clone())
                                .is_some();
                            if turn_running {
                                session.pending_interjections.push(PendingInterjection {
                                    text,
                                    attachments: images,
                                });
                                tracing::info!("Queued mid-turn interjection");
                            } else {
                                session
                                    .queue_interjection_fallback_prompt(text, images, true)
                                    .await;
                                SessionActor::maybe_start_running_task(
                                    session.clone(),
                                    completion_tx.clone(),
                                )
                                .await;
                            }
                        }
                        SessionCommand::GoalSummaryTurn { prompt_text } => {
                            // Queue a synthetic prompt so the model gets a turn
                            // to print a visible progress summary. Mirrors the
                            // pattern used by `maybe_drain_notifications`.
                            let prompt_id = format!("goal-summary-{}", uuid::Uuid::now_v7());
                            let prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(prompt_text))];
                            let (respond_to, _) = tokio::sync::oneshot::channel();
                            {
                                let mut state = session.state.lock().await;
                                state.pending_inputs.push_back(InputItem {
                                    prompt_id,
                                    prompt_blocks,
                                    prompt_mode: crate::session::plan_mode::PromptMode::Agent,
                                    trace_gcs_config: None,
                                    artifact_tracker: None,
                                    client_identifier: None,
                                    screen_mode: None,
                                    verbatim: true,
                                    json_schema: None,
                                    origin: super::PromptOrigin::GoalSummary,
                                    task_wake_fallback: None,
                                    tool_overrides_update: None,
                                    respond_to,
                                    persist_ack: None,
                                    parsed_prompt_tx: None,
                                    queue_meta: None,
                                    send_now: false,
                                });
                            }
                            SessionActor::maybe_start_running_task(session.clone(), completion_tx.clone()).await;
                        }
                        SessionCommand::WorkflowCompletionTurn { run_id, revision } => {
                            let state_suppressed = session.state.lock().await.notifications_suppressed;
                            let wake_suppressed = state_suppressed
                                || session.goal_loop_active()
                                || session
                                    .tool_context
                                    .task_wake_suppressed
                                    .as_ref()
                                    .is_some_and(|gate| gate.get());
                            let should_wake = if wake_suppressed {
                                false
                            } else {
                                let tracker = session.workflow_tracker().await;
                                tracker.lock().is_unreported_completion(&run_id, revision)
                            };
                            if !should_wake {
                                continue;
                            }
                            let prompt_id = format!("workflow-completed-{run_id}-{revision}");
                            let prompt_text = "A background workflow stopped. Review the workflow completion reminder, report the result to the user, and take any appropriate next action.";
                            let (respond_to, _) = tokio::sync::oneshot::channel();
                            {
                                let mut state = session.state.lock().await;
                                let workflow_wake_queued = state.pending_inputs.iter().any(|item| {
                                    matches!(item.origin, super::PromptOrigin::WorkflowCompleted { .. })
                                });
                                if workflow_wake_queued {
                                    continue;
                                }
                                state.pending_inputs.push_back(InputItem {
                                    prompt_id,
                                    prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(prompt_text))],
                                    prompt_mode: crate::session::plan_mode::PromptMode::Agent,
                                    trace_gcs_config: None,
                                    artifact_tracker: None,
                                    client_identifier: None,
                                    screen_mode: None,
                                    verbatim: true,
                                    json_schema: None,
                                    origin: super::PromptOrigin::WorkflowCompleted {
                                        completion_id: format!("{run_id}-{revision}"),
                                    },
                                    task_wake_fallback: None,
                                    tool_overrides_update: None,
                                    respond_to,
                                    persist_ack: None,
                                    parsed_prompt_tx: None,
                                    queue_meta: None,
                                    send_now: false,
                                });
                            }
                            SessionActor::maybe_start_running_task(session.clone(), completion_tx.clone()).await;
                        }
                        SessionCommand::TakeTurnMessages { respond_to } => {
                            let result = session.chat_state_handle.take_turn_messages().await;
                            let _ = respond_to.send(result);
                        }
                        SessionCommand::TakeHarnessTraceTurns { respond_to } => {
                            let result = session.chat_state_handle.take_harness_trace_turns().await;
                            let _ = respond_to.send(result);
                        }
                        SessionCommand::TakeStreamingCapture { prompt_id, respond_to } => {
                            // Out-of-band: never touches `chat_state`. The
                            // live slot is the only source of truth — there
                            // is no stash, so a queued prompt's
                            // `StreamStarted` racing this take will reset
                            // the slot to the new prompt-id and we'll log a
                            // tripwire before returning `None`.
                            let taken = {
                                let mut cap = session.streaming_turn_capture.lock();
                                if cap.prompt_id.as_deref() == Some(prompt_id.as_str()) {
                                    Some(std::mem::take(&mut *cap))
                                } else {
                                    // Race: live slot now belongs to a
                                    // different turn. Drop this take rather
                                    // than misattribute the partial. The
                                    // warn! is a production tripwire — if
                                    // we ever see it fire in real traffic
                                    // we should add a per-prompt stash.
                                    if !cap.is_empty() {
                                        tracing::warn!(
                                            requested_prompt_id = %prompt_id,
                                            slot_prompt_id = ?cap.prompt_id,
                                            "streaming_capture race: live slot belongs to a different prompt; \
                                             dropping streaming_partial.json for the requested turn",
                                        );
                                    }
                                    None
                                }
                            };
                            // Consolidate outside the lock — `finalize_for_upload`
                            // builds an up-to-8MB joined string, so it must not run
                            // while sampler events for a racing same-session turn
                            // contend for the mutex. Keep only uncommitted
                            // generations; empty afterwards ⇒ nothing to upload.
                            let result = taken.and_then(|mut cap| {
                                cap.finalize_for_upload();
                                (!cap.is_empty()).then_some(cap)
                            });
                            let _ = respond_to.send(result);
                        }
                        SessionCommand::PersistGitHead { commit, branch } => {
                            let _ = session.notifications.persistence_tx.send(
                                PersistenceMsg::GitHead { commit, branch },
                            );
                        }
                        SessionCommand::Shutdown => {
                            shutdown_workflows(&session).await;
                            // Flush the actor-owned replay buffer so any
                            // streamed chunks still pending at shutdown
                            // (e.g. reasoning text from a sampler stream
                            // racing with a CLI exit / harness teardown)
                            // are committed to updates.jsonl before the
                            // session directory is snapshotted for trace
                            // upload. Mirrors the same flush in the
                            // Cancel, CopyFile, and FlushComplete arms.
                            if let Some(notification) = replay_buffer.flush() {
                                session.emit_buffered(notification).await;
                            }
                            // Drop any queued synthetic auto-wake prompts and pending
                            // notifications before running hooks. Without this, a
                            // synthetic prompt that slipped through the per-tool-result
                            // sweep could still get flushed to chat_history.jsonl by
                            // any later persistence path, producing a trailing
                            // `<system-reminder>` with no assistant reply. Placed
                            // BEFORE hook dispatch so the cleanup runs even if hooks
                            // abort.
                            session.drop_pending_synthetic_items().await;

                            // ── session_end hook (shutdown path) ────────
                            // Fires BEFORE memory auto-save per plan contract.
                            let envelope = session.fire_hook(
                                xai_grok_hooks::event::HookEventName::SessionEnd,
                                None,
                                xai_grok_hooks::event::HookPayload::SessionEnd {
                                    reason: "shutdown".to_string(),
                                    turn_count: None,
                                    tool_call_count: None,
                                },
                            );
                            if let Some(registry) = session.hook_registry.borrow().clone() {
                                let ctx = session.hook_run_ctx();
                                let results = xai_grok_hooks::dispatcher::dispatch_non_blocking(
                                    &registry,
                                    xai_grok_hooks::event::HookEventName::SessionEnd,
                                    &envelope,
                                    &ctx,
                                )
                                .await;
                                session.send_hook_execution("session_end", None, None, &results).await;
                            }
                            session.dispatch_session_end_stop("shutdown").await;
                            // Memory: save session summary before shutdown
                            let mut session_end_result = "disabled";
                            let mut total_chunks_at_end = 0usize;
                            if !session.startup_hints.is_subagent {
                                if let Some(storage) = session.memory.storage() {
                                    let conversation = session.chat_state_handle.get_conversation().await;
                                    let result = crate::session::memory::hooks::on_session_end(
                                        &storage,
                                        &conversation,
                                        &session.session_info.id.0,
                                        session.memory.save_on_end,
                                    );
                                    session_end_result = match &result {
                                        crate::session::memory::hooks::SessionEndResult::Written(_) => "written",
                                        crate::session::memory::hooks::SessionEndResult::Skipped => "skipped",
                                        crate::session::memory::hooks::SessionEndResult::Failed(_) => "failed",
                                    };
                                    total_chunks_at_end = storage.total_chunk_count();
                                    let telem = session.memory.telemetry_snapshot();
                                    tracing::info!(
                                        target: xai_grok_telemetry::memory_log::TARGET,
                                        result = ?result,
                                        tool_searches = telem.tool_search_count,
                                        injection_searches = telem.injection_count,
                                        recovery_searches = telem.compaction_recovery_count,
                                        "MEMORY_SESSION_END: session summary saved"
                                    );
                                    // Reindex + embed the written file so it's searchable next session
                                    if let crate::session::memory::hooks::SessionEndResult::Written(ref path_str) = result {
                                        session.reindex_and_embed(std::path::Path::new(path_str), "session").await;
                                        session.send_xai_notification(XaiSessionUpdate::MemorySessionSaved {
                                            path: path_str.clone(),
                                        }).await;
                                    }
                                }
                            } else {
                                tracing::debug!(
                                    target: xai_grok_telemetry::memory_log::TARGET,
                                    "MEMORY_SUBAGENT_SKIP: skipping on_session_end for subagent session"
                                );
                            }
                            // Dream: attempt consolidation at session end
                            session.maybe_run_dream().await;
                            // Structured telemetry after dream so counters are populated
                            let telem = session.memory.telemetry_snapshot();
                            session.emit_memory_session_summary(&telem, total_chunks_at_end, session_end_result);
                            // Shutdown feedback sync loop and do final sync
                            if let Some(cancel) = &session.sync_loop_cancel {
                                cancel.cancel();
                            }
                            // Shutdown feedback manager (syncs signals, drains upload queue)
                            session.feedback_manager.shutdown(session.upload_queue.get()).await;
                            if !session.startup_hints.is_subagent {
                                session.persist_background_task_manifest().await;
                            }
                            // Clean up scratch directory (pre-edit file copies).
                            cleanup_session_scratch(&session);
                            return;
                        }
                    }
            }
        }
    }
}
/// Extract the user query text and assistant response text for the
/// `turn_number`-th turn (0-based) of a conversation snapshot. Used by
/// the `GetFeedbackContext` handler when a client supplies a `turn_number`
/// (per-turn thumbs button on a specific assistant message).
pub(super) fn turn_texts_for_feedback(
    conversation: &[xai_grok_sampling_types::ConversationItem],
    turn_number: usize,
) -> (Option<String>, Option<String>) {
    use xai_grok_sampling_types::ConversationItem;
    let Some(start) = conversation
        .iter()
        .enumerate()
        .filter(|(_, item)| matches!(item, ConversationItem::User(_)))
        .nth(turn_number)
        .map(|(i, _)| i)
    else {
        return (None, None);
    };
    let raw = conversation[start].text_content();
    let extracted = xai_chat_state::compaction_utils::extract_user_query(&raw);
    let user_text = (!extracted.is_empty()).then_some(extracted);
    let assistant_text = conversation
        .iter()
        .skip(start + 1)
        .take_while(|item| !matches!(item, ConversationItem::User(_)))
        .find_map(|item| {
            if let ConversationItem::Assistant(a) = item
                && !a.content.trim().is_empty()
            {
                Some(a.content.as_ref().to_owned())
            } else {
                None
            }
        });
    (user_text, assistant_text)
}
