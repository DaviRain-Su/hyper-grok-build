//! Outbound update emission concern for `SessionActor`: `send_update` and
//! its buffered/transient/direct variants, xAI-notification handling, and
//! the gateway-bridge dispatch shims.
use super::*;
/// Result of applying a subagent fold into parent ledgers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SubagentUsageApply {
    /// Tokens attributed to the live open prompt (and session).
    AttributedToPrompt,
    /// Tokens landed on the session ledger only (pin mismatch / no live pin).
    /// Sticky report only — do not stain ledgers for "missing" spend.
    SessionOnly,
}
impl SessionActor {
    /// Apply subagent usage. `Ok` after chat-state acked; `Err` if apply failed.
    pub(super) async fn record_subagent_usage(
        &self,
        by_model: &[(String, xai_chat_state::UsageTotals)],
        parent_prompt_id: Option<&str>,
        incomplete: bool,
    ) -> Result<SubagentUsageApply, ()> {
        if by_model.is_empty() && !incomplete {
            return Ok(SubagentUsageApply::AttributedToPrompt);
        }
        let current = self
            .current_prompt_id
            .lock()
            .expect("current_prompt_id mutex poisoned")
            .clone();
        let attributable = parent_prompt_id.is_some() && parent_prompt_id == current.as_deref();
        if !self
            .chat_state_handle
            .record_subagent_usage(by_model.to_vec(), attributable, incomplete)
            .await
        {
            return Err(());
        }
        Ok(if attributable {
            SubagentUsageApply::AttributedToPrompt
        } else {
            SubagentUsageApply::SessionOnly
        })
    }
    /// True-miss / unpinned fail-closed: sticky for freeze report + pin-aware
    /// ledger marks. Prompt ledger is stained only when the stamped pin is the
    /// live open prompt (never stain a different live turn). Session always.
    pub(super) async fn mark_apply_miss_incomplete(&self, stamped_pin: Option<&str>) -> bool {
        let sticky = self.mark_subagent_usage_not_applied(stamped_pin).await;
        let live = self.current_prompt_id.lock().ok().and_then(|g| g.clone());
        let stain_prompt = match (stamped_pin, live.as_deref()) {
            (Some(pin), Some(live_id)) => pin == live_id,
            (Some(_), None) => false,
            (None, Some(_)) => true,
            (None, None) => false,
        };
        let ledger_ok = self
            .chat_state_handle
            .mark_usage_incomplete(stain_prompt, true)
            .await;
        sticky || ledger_ok
    }
    /// Shared freeze/cancel finalize: ledger marks only on `fail_closed`;
    /// sticky/bg are report-only. Clears sticky after snapshot.
    pub(super) async fn finalize_usage_from_outcome(
        &self,
        prompt_id: &str,
        outcome: super::turn::UsageDrainOutcome,
    ) -> Option<crate::extensions::notification::PromptUsage> {
        if outcome.fail_closed {
            let _ = self
                .chat_state_handle
                .mark_usage_incomplete(true, true)
                .await;
        }
        let usage = self
            .snapshot_prompt_usage_marked(outcome.report_incomplete())
            .await;
        self.clear_subagent_usage_not_applied(prompt_id);
        usage
    }
    /// Sends an update to the persistence layer and the gateway.
    /// Optionally includes a `chunk_index` for LLM streaming chunk tracking.
    pub(super) async fn send_update(&self, update: acp::SessionUpdate, chunk_index: Option<u64>) {
        self.send_update_full(update, chunk_index, None, false)
            .await;
    }
    async fn send_update_full(
        &self,
        update: acp::SessionUpdate,
        chunk_index: Option<u64>,
        agent_timestamp_ms_override: Option<i64>,
        is_replay: bool,
    ) {
        self.close_rewind_window().await;
        if let acp::SessionUpdate::ToolCall(tool_call) = &update
            && matches!(tool_call.kind, acp::ToolKind::Edit)
        {
            let cwd = self.tool_context.cwd.as_path();
            for loc in &tool_call.locations {
                let mut p = loc.path.clone();
                if p.is_absolute() {
                    if let Ok(rel) = p.strip_prefix(cwd) {
                        p = rel.to_path_buf();
                    } else {
                        continue;
                    }
                }
                if !p.as_os_str().is_empty() {
                    self.chat_state_handle
                        .record_agent_edited_path(p.to_string_lossy().to_string());
                }
            }
        }
        let total_tokens = self.chat_state_handle.get_estimated_total_tokens().await;
        let meta_info = self.chat_state_handle.get_notification_meta().await;
        let (stream_start_ms, turn_start_ms) = meta_info
            .map(|m| (m.stream_start_ms, m.turn_start_ms))
            .unwrap_or((None, None));
        let event_id = self.generate_event_id();
        let agent_timestamp_ms =
            agent_timestamp_ms_override.unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let (update_type, update_params) = Self::extract_update_info(&update);
        let mut meta = json!({
            "totalTokens": total_tokens,
            "eventId": event_id,
            "agentTimestampMs": agent_timestamp_ms,
        });
        let obj = meta
            .as_object_mut()
            .expect("json! literal is always an Object");
        if let Some(pid) = self.current_prompt_id.lock().ok().and_then(|g| g.clone()) {
            obj.insert("promptId".to_string(), pid.into());
        }
        if let Some(ms) = stream_start_ms {
            obj.insert("streamStartMs".to_string(), ms.into());
        }
        if let Some(ms) = turn_start_ms {
            obj.insert("turnStartMs".to_string(), ms.into());
        }
        if let Some(update_type) = update_type {
            obj.insert("updateType".to_string(), update_type.into());
        }
        if let Some(update_params) = update_params {
            obj.insert("updateParams".to_string(), update_params);
        }
        if let Some(idx) = chunk_index {
            obj.insert("chunkId".to_string(), idx.into());
        }
        if is_replay {
            obj.insert("isReplay".to_string(), true.into());
        }
        let notification = acp::SessionNotification::new(self.session_info.id.clone(), update)
            .meta(meta.as_object().cloned());
        let _ = self
            .event_tx
            .send(SessionEvent::Notification(notification.into()));
    }
    /// Producer for the **high-frequency streaming path** with an xAI
    /// extension payload. Routes through `event_tx` -> `ReplayBuffer` ->
    /// `emit_buffered` so chunks get merged + debounced + emitted.
    ///
    /// For one-shot xAI events (RetryState, ImageCompressed, HookExecution,
    /// AutoCompactCompleted, etc.), use `send_xai_notification` instead.
    ///
    /// The frequency-based split (`send_buffered_xai_update` vs `send_xai_notification`)
    /// mirrors the ACP-side split between `send_update` (high-frequency,
    /// buffered) and `emit_notification_direct` (low-frequency, direct).
    pub(super) async fn send_buffered_xai_update(&self, update: XaiSessionUpdate) {
        self.close_rewind_window().await;
        let notification = XaiSessionNotification {
            session_id: self.session_info.id.clone(),
            update,
            meta: None,
        };
        let _ = self
            .event_tx
            .send(SessionEvent::Notification(notification.into()));
    }
    /// Enqueue a `CurrentModeUpdate` on the FIFO event pipeline, stamped at
    /// enqueue time like `send_update`, so its id is minted in delivery order
    /// relative to already-queued chunks. A direct `emit_notification_direct`
    /// here would mint a HIGHER id that is delivered BEFORE those chunks, and
    /// the client's in-order dedup would then drop the chunks as stale —
    /// silent text loss on a mid-stream plan-mode toggle. Persist + broadcast
    /// happen when the actor loop drains the event through `emit_buffered`.
    pub(super) fn enqueue_current_mode_update(&self, current_mode_id: acp::SessionModeId) {
        let notification = acp::SessionNotification::new(
            self.session_info.id.clone(),
            acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new(current_mode_id)),
        )
        .meta(self.build_notification_meta().as_object().cloned());
        let _ = self
            .event_tx
            .send(SessionEvent::Notification(notification.into()));
    }
    /// Emit a notification that has come out of the **high-frequency
    /// streaming path** (after the `ReplayBuffer` has decided to flush
    /// it). Single dispatch point that routes by inner protocol kind:
    ///
    /// - **ACP** (`AgentMessageChunk`, `AgentThoughtChunk`) ->
    ///   delegates to `emit_notification_direct` (persists + gateway).
    /// - **xAI** (`ToolCallDeltaChunk`) -> inlines a gateway
    ///   forward as `ExtNotification` only. Two deliberate omissions:
    ///   (1) no persistence -- per-chunk deltas have no replay value
    ///   because the canonical `acp::SessionUpdate::ToolCall` (with
    ///   assembled `raw_input`) is persisted at end-of-turn and is the
    ///   source of truth for replay; (2) no hook dispatch.
    pub(super) async fn emit_buffered(&self, notification: SessionNotification) {
        match notification {
            SessionNotification::Acp(n) => {
                self.emit_notification_direct(*n).await;
            }
            SessionNotification::Xai(n) => {
                self.log_outbound_xai_buffered(&n);
                if self
                    .notifications
                    .gateway_enabled
                    .load(std::sync::atomic::Ordering::Relaxed)
                    && let Ok(value) = serde_json::to_value(&*n)
                    && let Ok(params) = serde_json::value::to_raw_value(&value)
                {
                    self.notifications
                        .gateway
                        .forward_fire_and_forget(acp::ExtNotification::new(
                            "x.ai/session_notification",
                            params.into(),
                        ));
                }
            }
        }
    }
    /// Tracing log for buffered xAI notifications emerging from
    /// emit_buffered. Mirrors `log_outbound_notification` for ACP.
    /// Visible with `RUST_LOG=acp_event=info`.
    fn log_outbound_xai_buffered(&self, notification: &XaiSessionNotification) {
        if !matches!(
            notification.update,
            XaiSessionUpdate::ToolCallDeltaChunk { .. }
        ) {
            return;
        }
        tracing::info!(
            target: "acp_event",
            event = "xai_buffered_notification_sent",
            session_id = %self.session_info.id,
            "Sending buffered xAI session notification"
        );
    }
    fn log_outbound_notification(&self, notification: &acp::SessionNotification) {
        let meta = notification.meta.as_ref();
        let event_id = meta
            .and_then(|m| m.get("eventId"))
            .and_then(|v| v.as_str())
            .unwrap_or("<missing>");
        let agent_timestamp_ms = meta
            .and_then(|m| m.get("agentTimestampMs"))
            .and_then(|v| v.as_i64().or_else(|| v.as_u64().map(|u| u as i64)))
            .unwrap_or(0);
        let update_type = meta
            .and_then(|m| m.get("updateType"))
            .and_then(|v| v.as_str())
            .unwrap_or("<missing>");
        let chunk_index = meta
            .and_then(|m| m.get("chunkIndex"))
            .and_then(|v| v.as_u64());
        tracing::info!(
            target: "acp_event",
            event = "agent_message_sent",
            event_id = %event_id,
            session_id = %self.session_info.id,
            agent_timestamp_ms = agent_timestamp_ms,
            update_type = %update_type,
            chunk_index = ?chunk_index,
            "Sending session update"
        );
    }
    pub(crate) async fn emit_notification_direct(
        &self,
        mut notification: acp::SessionNotification,
    ) {
        crate::util::event_id::ensure_event_id_meta(
            &self.session_info.id.0,
            &mut notification.meta,
        );
        self.log_outbound_notification(&notification);
        if !matches!(
            notification.update,
            acp::SessionUpdate::AvailableCommandsUpdate(_)
        ) {
            let _ = self
                .notifications
                .persistence_tx
                .send(PersistenceMsg::Update(
                    crate::session::storage::SessionUpdate::Acp(Box::new(notification.clone())),
                ));
        }
        if self
            .notifications
            .gateway_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            self.notifications
                .gateway
                .forward_fire_and_forget(notification);
        }
    }
    /// Send a notification to the live client **without persisting** it.
    ///
    /// Use this for cosmetic/transient UI updates (e.g., turn-end plan
    /// cleanup) that should NOT be replayed on session reload.  The
    /// underlying resource state is the source of truth; this only
    /// adjusts what the live client sees right now.
    pub(super) fn emit_transient_notification(&self, notification: acp::SessionNotification) {
        self.log_outbound_notification(&notification);
        if self
            .notifications
            .gateway_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            self.notifications
                .gateway
                .forward_fire_and_forget(notification);
        }
    }
    /// Flush buffered notifications and drain the persistence merge buffer to
    /// disk. Blocks until the persistence actor confirms the write is complete.
    ///
    /// Must NOT be called from within `run_session()` — the flush goes through
    /// `event_tx`, which is consumed by the same select loop (deadlock / 5s timeout).
    pub(super) async fn flush_to_disk(&self) {
        if let Err(e) = crate::session::replay_events::flush_replay_actor(&self.event_tx).await {
            tracing::warn!(?e, "flush_replay_actor failed");
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::FlushAndAck { respond_to: tx })
            .is_ok()
        {
            let _ = rx.await;
        }
    }
    /// Extracts the update type name and relevant parameters for logging
    fn extract_update_info(
        update: &acp::SessionUpdate,
    ) -> (Option<String>, Option<serde_json::Value>) {
        match update {
            acp::SessionUpdate::UserMessageChunk(_) => (Some("UserMessageChunk".to_string()), None),
            acp::SessionUpdate::AgentMessageChunk(_) => {
                (Some("AgentMessageChunk".to_string()), None)
            }
            acp::SessionUpdate::AgentThoughtChunk(_) => {
                (Some("AgentThoughtChunk".to_string()), None)
            }
            acp::SessionUpdate::ToolCall(tool_call) => (
                Some("ToolCall".to_string()),
                Some(json!({
                    "toolCallId": tool_call.tool_call_id.0,
                    "title": tool_call.title,
                    "kind": format!("{:?}", tool_call.kind),
                    "status": format!("{:?}", tool_call.status),
                })),
            ),
            acp::SessionUpdate::ToolCallUpdate(tool_update) => (
                Some("ToolCallUpdate".to_string()),
                Some(json!({
                    "toolCallId": tool_update.tool_call_id.0,
                    "status": tool_update.fields.status.as_ref().map(|s| format!("{:?}", s)),
                })),
            ),
            acp::SessionUpdate::Plan(plan) => (
                Some("Plan".to_string()),
                Some(json!({
                    "planSteps": plan.entries.len(),
                })),
            ),
            acp::SessionUpdate::AvailableCommandsUpdate(update) => (
                Some("AvailableCommandsUpdate".to_string()),
                Some(json!({
                    "commandsCount": update.available_commands.len(),
                })),
            ),
            acp::SessionUpdate::CurrentModeUpdate(update) => (
                Some("CurrentModeUpdate".to_string()),
                Some(json!({
                    "currentModeId": update.current_mode_id,
                })),
            ),
            _ => (None, None),
        }
    }
    /// Generates a unique event ID for correlation across agent/relay/client
    fn generate_event_id(&self) -> String {
        crate::util::event_id::generate_event_id(&self.session_info.id.0)
    }
    /// Builds notification meta with event ID and timestamp.
    /// Use this for all notifications (including user message chunks) to ensure
    /// consistent event ID format for deduplication in the relay.
    pub(super) fn build_notification_meta(&self) -> serde_json::Value {
        let event_id = self.generate_event_id();
        let agent_timestamp_ms = chrono::Utc::now().timestamp_millis();
        json!({
            "eventId": event_id,
            "agentTimestampMs": agent_timestamp_ms,
        })
    }
    /// Handle xAI session notifications - store them in persistence
    /// These are client-side events (like diff reviews) that should be part of session history.
    /// Exception: `SubagentProgress` ticks are transient and return before the store.
    pub(super) async fn handle_xai_session_notification(
        &self,
        mut notification: XaiSessionNotification,
    ) {
        if !matches!(
            notification.update,
            XaiSessionUpdate::SubagentProgress { .. }
        ) {
            tracing::debug!("storing xAI session notification");
        }
        {
            let mut meta_map = notification.meta.take().and_then(|v| match v {
                serde_json::Value::Object(m) => Some(m),
                _ => None,
            });
            crate::util::event_id::ensure_event_id_meta(&self.session_info.id.0, &mut meta_map);
            notification.meta = meta_map.map(serde_json::Value::Object);
        }
        match &notification.update {
            XaiSessionUpdate::SubagentSpawned {
                subagent_id,
                subagent_type,
                description,
                resumed_from,
                model,
                ..
            } => {
                if let Some(parent_id) = resumed_from {
                    debug_assert_ne!(parent_id, subagent_id, "subagent cannot resume itself");
                }
                {
                    let goal_id = self
                        .goal_tracker
                        .lock()
                        .snapshot()
                        .map(|o| o.goal_id.clone());
                    let mut records = self.subagent_token_records.lock();
                    let anchor = resumed_from
                        .as_deref()
                        .map(|pid| match records.get(pid) {
                            Some(r) => r.last_cumulative_reported,
                            None => {
                                tracing::debug!(
                                    parent_id = %pid,
                                    subagent_id = %subagent_id,
                                    "resume parent not in token registry; anchoring at 0"
                                );
                                0
                            }
                        })
                        .unwrap_or(0);
                    debug_assert!(
                        !records.contains_key(subagent_id),
                        "duplicate SubagentSpawned for {subagent_id}"
                    );
                    records.insert(
                        subagent_id.clone(),
                        SubagentTokenRecord {
                            goal_id,
                            resume_anchor_cumulative: anchor,
                            last_cumulative_reported: anchor,
                            model: model.clone(),
                            finished: false,
                        },
                    );
                }
                if self.goal_harness_enabled() {
                    let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
                    if !self.goal_runs_on_workflow_engine() {
                        self.drain_goal_updates(current_tokens, DrainPurpose::MidTurn)
                            .await;
                    }
                    let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
                    let notify = self.goal_notify_sender();
                    notify.emit_goal_updated(
                        &mut self.goal_tracker.lock(),
                        tokens_used,
                        finished_marginal,
                    );
                }
                let envelope = self.fire_hook(
                    xai_grok_hooks::event::HookEventName::SubagentStart,
                    None,
                    xai_grok_hooks::event::HookPayload::SubagentStart {
                        subagent_id: subagent_id.clone(),
                        subagent_type: subagent_type.clone(),
                        description: Some(description.clone()),
                    },
                );
                let hook_registry_snapshot = self.hook_registry.borrow().clone();
                if let Some(registry) = hook_registry_snapshot {
                    let ctx = self.hook_run_ctx();
                    let _ = xai_grok_hooks::dispatcher::dispatch_non_blocking(
                        &registry,
                        xai_grok_hooks::event::HookEventName::SubagentStart,
                        &envelope,
                        &ctx,
                    )
                    .await;
                }
            }
            XaiSessionUpdate::SubagentFinished {
                subagent_id,
                tokens_used,
                ..
            } => {
                {
                    let mut records = self.subagent_token_records.lock();
                    if let Some(rec) = records.get_mut(subagent_id) {
                        rec.last_cumulative_reported =
                            rec.last_cumulative_reported.max(*tokens_used);
                        rec.finished = true;
                    }
                }
                {
                    let mut tracker = self.goal_tracker.lock();
                    if let Some(o) = tracker.snapshot_mut() {
                        o.live_subagent_tokens = 0;
                        o.live_context_pct = 0;
                        o.live_turn_count = 0;
                        o.live_tool_call_count = 0;
                        o.live_tokens_by_model.clear();
                    }
                }
                if self.goal_harness_enabled() && self.goal_tracker.lock().snapshot().is_some() {
                    let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
                    let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
                    let notify = self.goal_notify_sender();
                    notify.emit_goal_updated(
                        &mut self.goal_tracker.lock(),
                        tokens_used,
                        finished_marginal,
                    );
                }
            }
            XaiSessionUpdate::SubagentProgress {
                subagent_id,
                turn_count,
                tool_call_count,
                tokens_used,
                context_window_tokens,
                context_usage_pct,
                ..
            } => {
                let goal_id = self
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .map(|o| o.goal_id.clone());
                let progress = {
                    let mut records = self.subagent_token_records.lock();
                    match records.get_mut(subagent_id) {
                        Some(rec)
                            if !rec.finished && goal_id.is_some() && rec.goal_id == goal_id =>
                        {
                            let advanced = *tokens_used > rec.last_cumulative_reported;
                            rec.last_cumulative_reported =
                                rec.last_cumulative_reported.max(*tokens_used);
                            Some((advanced, rec.last_cumulative_reported))
                        }
                        Some(_) => None,
                        None => {
                            tracing::debug!(
                                subagent_id = %subagent_id,
                                "progress tick for unregistered subagent; dropped"
                            );
                            None
                        }
                    }
                };
                if let Some((advanced, ratcheted_tokens)) = progress
                    && self.goal_harness_enabled()
                {
                    let model_id = self
                        .chat_state_handle
                        .get_sampling_config()
                        .await
                        .map(|c| c.model)
                        .unwrap_or_default();
                    let tokens_by_model = self.goal_tokens_by_model(&model_id);
                    self.goal_tracker.lock().update_live_progress(
                        ratcheted_tokens,
                        tokens_by_model,
                        *context_window_tokens,
                        *context_usage_pct,
                        *turn_count,
                        *tool_call_count,
                    );
                    if advanced {
                        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
                        let (goal_tokens_used, finished_marginal) =
                            self.goal_tokens(current_tokens);
                        let notify = self.goal_notify_sender();
                        notify.emit_goal_updated_ephemeral(
                            &mut self.goal_tracker.lock(),
                            goal_tokens_used,
                            finished_marginal,
                        );
                    }
                }
                return;
            }
            _ => {}
        }
        let _ = self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::Update(
                crate::session::storage::SessionUpdate::Xai(Box::new(notification)),
            ));
    }
    /// Persist an xAI extension notification to `updates.jsonl` **without** sending it
    /// to the gateway/UI. Used for internal bookkeeping updates like `CompactionCheckpoint`
    /// and `RewindMarker` that are only relevant during replay.
    pub(super) fn persist_xai_update_only(&self, update: XaiSessionUpdate) {
        let notification = XaiSessionNotification {
            session_id: self.session_info.id.clone(),
            update,
            meta: Some(self.build_notification_meta()),
        };
        if self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::Update(
                crate::session::storage::SessionUpdate::Xai(Box::new(notification)),
            ))
            .is_err()
        {
            tracing::warn!("Failed to send xAI update to persistence channel");
        }
    }
    /// Dispatch a `Notification` hook for a user-attention event.
    pub(super) async fn dispatch_notification_hook(
        &self,
        notification_type: &str,
        message: Option<String>,
        title: Option<String>,
        level: Option<String>,
    ) {
        let envelope = self.fire_hook(
            xai_grok_hooks::event::HookEventName::Notification,
            None,
            xai_grok_hooks::event::HookPayload::Notification {
                notification_type: notification_type.to_string(),
                message,
                title,
                level,
            },
        );
        let hook_registry_snapshot = self.hook_registry.borrow().clone();
        let Some(registry) = hook_registry_snapshot else {
            return;
        };
        let ctx = self.hook_run_ctx();
        let _ = xai_grok_hooks::dispatcher::dispatch_non_blocking(
            &registry,
            xai_grok_hooks::event::HookEventName::Notification,
            &envelope,
            &ctx,
        )
        .await;
    }
    /// Send an xAI extension notification to the client
    #[tracing::instrument(skip_all)]
    pub(super) async fn send_xai_notification(&self, update: XaiSessionUpdate) {
        self.send_xai_notification_with_extra_meta(update, None)
            .await;
    }
    /// [`Self::send_xai_notification`] with caller-supplied `_meta` keys merged
    /// into the standard eventId/timestamp meta. Caller keys win on collision.
    #[tracing::instrument(skip_all)]
    pub(super) async fn send_xai_notification_with_extra_meta(
        &self,
        update: XaiSessionUpdate,
        extra_meta: Option<serde_json::Map<String, serde_json::Value>>,
    ) {
        self.close_rewind_window().await;
        let meta = {
            let mut meta = self.build_notification_meta();
            if let (Some(obj), Some(extra)) = (meta.as_object_mut(), extra_meta) {
                obj.extend(extra);
            }
            meta
        };
        let notification = XaiSessionNotification {
            session_id: self.session_info.id.clone(),
            update,
            meta: Some(meta),
        };
        let _ = self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::Update(
                crate::session::storage::SessionUpdate::Xai(Box::new(notification.clone())),
            ));
        let params = serde_json::to_value(&notification)
            .and_then(|v| serde_json::value::to_raw_value(&v))
            .ok();
        if let Some(params) = params {
            let ext_notification =
                acp::ExtNotification::new("x.ai/session_notification", params.into());
            self.notifications
                .gateway
                .forward_fire_and_forget(ext_notification);
        }
        if let Some((notification_type, message, title, level)) =
            notification_hook_for_update(&notification.update)
        {
            self.dispatch_notification_hook(&notification_type, message, title, level)
                .await;
        }
    }
}
#[cfg(test)]
mod xai_event_id_stamping_tests {
    use super::support::create_test_actor;
    use super::*;
    fn persisted_xai_event_id(
        prx: &mut tokio::sync::mpsc::UnboundedReceiver<PersistenceMsg>,
    ) -> String {
        loop {
            match prx.try_recv().expect("an xAI line must be persisted") {
                PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(notif)) => {
                    return notif
                        .meta
                        .as_ref()
                        .and_then(|m| m.get("eventId"))
                        .and_then(|v| v.as_str())
                        .expect("persisted xAI lines must carry an eventId")
                        .to_string();
                }
                _ => continue,
            }
        }
    }
    /// Persisted⇒stamped chokepoint at the actor: both actor persist paths —
    /// `send_xai_notification` (own emission) and
    /// `handle_xai_session_notification` (inbound/forwarded, meta-less) —
    /// must put an `eventId` on the persisted line. An id-less line degrades
    /// every later cursor reconnect of the session to a full replay.
    #[tokio::test]
    async fn actor_persisted_xai_lines_carry_event_id() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) =
                    tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
                let (persistence_tx, mut prx) =
                    tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
                let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
                actor
                    .send_xai_notification(XaiSessionUpdate::HookAnnotation {
                        message: "own emission".into(),
                    })
                    .await;
                let own_id = persisted_xai_event_id(&mut prx);
                assert!(own_id.starts_with("test-actor-"));
                actor
                    .handle_xai_session_notification(XaiSessionNotification {
                        session_id: acp::SessionId::new("test-actor"),
                        update: XaiSessionUpdate::HookAnnotation {
                            message: "inbound".into(),
                        },
                        meta: None,
                    })
                    .await;
                let inbound_id = persisted_xai_event_id(&mut prx);
                assert!(inbound_id.starts_with("test-actor-"));
                assert_ne!(own_id, inbound_id);
                actor.persist_xai_update_only(XaiSessionUpdate::HookAnnotation {
                    message: "persist-only".into(),
                });
                let persist_only_id = persisted_xai_event_id(&mut prx);
                assert!(persist_only_id.starts_with("test-actor-"));
                assert_ne!(inbound_id, persist_only_id);
            })
            .await;
    }
    /// `emit_notification_direct` is the actor's ACP persist/broadcast fork:
    /// it must stamp any direct caller that didn't stamp at enqueue (none
    /// exist today — this is the safety net for the next one), so every
    /// persisted ACP line stays cursor-addressable.
    #[tokio::test]
    async fn emit_notification_direct_stamps_unstamped_acp_lines() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) =
                    tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
                let (persistence_tx, mut prx) =
                    tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
                let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
                actor
                    .emit_notification_direct(acp::SessionNotification::new(
                        acp::SessionId::new("test-actor"),
                        acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new(
                            acp::SessionModeId::new("plan"),
                        )),
                    ))
                    .await;
                match prx.try_recv().expect("must persist") {
                    PersistenceMsg::Update(crate::session::storage::SessionUpdate::Acp(notif)) => {
                        assert!(
                            notif
                                .meta
                                .as_ref()
                                .and_then(|m| m.get("eventId"))
                                .and_then(|v| v.as_str())
                                .is_some_and(|id| id.starts_with("test-actor-")),
                            "the chokepoint must stamp meta-less ACP notifications"
                        );
                    }
                    _ => panic!("expected Acp update"),
                }
            })
            .await;
    }
    /// Mid-stream plan toggle: the plan-mode `CurrentModeUpdate` must ride
    /// the FIFO event pipeline BEHIND already-queued chunks, with its id
    /// minted at ENQUEUE time. A direct emit would mint a higher id yet
    /// deliver/persist first, and the client's in-order ACP dedup would then
    /// drop the queued chunks as stale (silent text loss).
    ///
    /// Pins the enter AND exit legs of `handle_session_mode` (each must emit —
    /// dropping either `enqueue_current_mode_update` call loses the client's
    /// mode confirmation). The abandoned site shares the same helper but needs
    /// an `ext_method` round-trip harness to drive, so it is not pinned here.
    #[tokio::test]
    async fn plan_mode_current_mode_update_rides_event_pipeline_in_id_order() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) =
                    tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
                let (persistence_tx, mut prx) =
                    tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
                let (actor, mut event_rx) = super::support::create_test_actor_ex(
                    0,
                    256_000,
                    85,
                    gateway_tx,
                    persistence_tx,
                )
                .await;
                actor
                    .send_update(
                        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                            acp::ContentBlock::Text(acp::TextContent::new("queued text")),
                        )),
                        None,
                    )
                    .await;
                actor
                    .handle_session_mode(acp::SessionModeId::new("plan"))
                    .await;
                while let Ok(msg) = prx.try_recv() {
                    assert!(
                        !matches!(msg, PersistenceMsg::Update(_)),
                        "the mode update must not short-circuit the event queue"
                    );
                }
                let mut queued = Vec::new();
                while let Ok(event) = event_rx.try_recv() {
                    match event {
                        SessionEvent::Notification(n) => queued.push(n),
                        other => {
                            panic!("expected only Notification events, got {other:?}")
                        }
                    }
                }
                assert_eq!(queued.len(), 2, "chunk + mode update must be queued");
                match &queued[1] {
                    SessionNotification::Acp(n) => {
                        assert!(matches!(n.update, acp::SessionUpdate::CurrentModeUpdate(_)));
                        assert!(
                            n.meta.as_ref().and_then(|m| m.get("eventId")).is_some(),
                            "the queued mode update must already carry its enqueue-time id"
                        );
                    }
                    other => {
                        panic!("expected the mode update behind the chunk, got {other:?}")
                    }
                }
                let mut replay_buffer = crate::agent::update_chunk_merge::ReplayBuffer::new(
                    actor.buffering_settings.clone(),
                );
                for notification in queued {
                    if let Some((primary, secondary)) = replay_buffer.consume_chunk(notification) {
                        actor.emit_buffered(primary).await;
                        if let Some(extra) = secondary {
                            actor.emit_buffered(extra).await;
                        }
                    }
                }
                let numeric_seq = |n: &acp::SessionNotification| -> u64 {
                    n.meta
                        .as_ref()
                        .and_then(|m| m.get("eventId"))
                        .and_then(|v| v.as_str())
                        .and_then(|id| id.rsplit('-').next())
                        .and_then(|s| s.parse().ok())
                        .expect("persisted ACP lines must carry a numeric eventId")
                };
                let mut persisted = Vec::new();
                while let Ok(msg) = prx.try_recv() {
                    if let PersistenceMsg::Update(crate::session::storage::SessionUpdate::Acp(n)) =
                        msg
                    {
                        persisted.push(*n);
                    }
                }
                assert_eq!(persisted.len(), 2, "both lines must persist on drain");
                assert!(matches!(
                    persisted[0].update,
                    acp::SessionUpdate::AgentMessageChunk(_)
                ));
                assert!(matches!(
                    persisted[1].update,
                    acp::SessionUpdate::CurrentModeUpdate(_)
                ));
                assert!(
                    numeric_seq(&persisted[0]) < numeric_seq(&persisted[1]),
                    "delivery order must match id order — the dedup premise"
                );
                actor
                    .send_update(
                        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                            acp::ContentBlock::Text(acp::TextContent::new("queued before exit")),
                        )),
                        None,
                    )
                    .await;
                actor
                    .handle_session_mode(acp::SessionModeId::new("default"))
                    .await;
                while let Ok(msg) = prx.try_recv() {
                    assert!(
                        !matches!(msg, PersistenceMsg::Update(_)),
                        "the exit mode update must not short-circuit the event queue"
                    );
                }
                let mut queued = Vec::new();
                while let Ok(event) = event_rx.try_recv() {
                    match event {
                        SessionEvent::Notification(n) => queued.push(n),
                        other => {
                            panic!("expected only Notification events, got {other:?}")
                        }
                    }
                }
                assert_eq!(queued.len(), 2, "chunk + exit mode update must be queued");
                match &queued[1] {
                    SessionNotification::Acp(n) => match &n.update {
                        acp::SessionUpdate::CurrentModeUpdate(cmu) => {
                            assert_eq!(
                                cmu.current_mode_id.0.as_ref(),
                                "default",
                                "the exit emission must carry the new mode id"
                            );
                            assert!(
                                n.meta.as_ref().and_then(|m| m.get("eventId")).is_some(),
                                "the queued exit mode update must carry its enqueue-time id"
                            );
                        }
                        other => panic!("expected CurrentModeUpdate, got {other:?}"),
                    },
                    other => {
                        panic!("expected the mode update behind the chunk, got {other:?}")
                    }
                }
                for notification in queued {
                    if let Some((primary, secondary)) = replay_buffer.consume_chunk(notification) {
                        actor.emit_buffered(primary).await;
                        if let Some(extra) = secondary {
                            actor.emit_buffered(extra).await;
                        }
                    }
                }
                let mut persisted = Vec::new();
                while let Ok(msg) = prx.try_recv() {
                    if let PersistenceMsg::Update(crate::session::storage::SessionUpdate::Acp(n)) =
                        msg
                    {
                        persisted.push(*n);
                    }
                }
                assert_eq!(persisted.len(), 2, "exit leg must persist both lines");
                assert!(matches!(
                    persisted[1].update,
                    acp::SessionUpdate::CurrentModeUpdate(_)
                ));
                assert!(
                    numeric_seq(&persisted[0]) < numeric_seq(&persisted[1]),
                    "exit-leg delivery order must match id order too"
                );
            })
            .await;
    }
}
