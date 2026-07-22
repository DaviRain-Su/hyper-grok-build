use super::*;

/// Running-turn display fields for `x.ai/queue/changed` (clients paint turn-start UI).
pub(super) struct RunningPromptDisplay {
    pub id: String,
    pub text: String,
    pub kind: String,
    pub combined_texts: Option<Vec<String>>,
}

impl SessionActor {
    /// Queue a user-originated prompt (writes to prompt history).
    ///
    /// `send_now` (or a user prompt arriving during an interruptible wait)
    /// inserts the prompt to run next. Returns `true` when the caller must
    /// cancel the running turn.
    pub(super) async fn queue_input(
        &self,
        prompt_blocks: Vec<acp::ContentBlock>,
        prompt_id: String,
        prompt_mode: PromptMode,
        trace_gcs_config: Option<crate::session::repo_changes::TraceExportConfig>,
        artifact_tracker: Option<crate::upload::manifest::ArtifactTracker>,
        client_identifier: Option<String>,
        screen_mode: Option<String>,
        verbatim: bool,
        json_schema: Option<serde_json::Value>,
        send_now: bool,
        task_wake_fallback: Option<TaskWakeFallback>,
        respond_to: oneshot::Sender<PromptTurnResult>,
        persist_ack: Option<oneshot::Sender<()>>,
        parsed_prompt_tx: Option<oneshot::Sender<ParsedPromptInfo>>,
    ) -> bool {
        tracing::info!("queueing prompt: {prompt_id}");
        let queue_depth = { self.state.lock().await.pending_inputs.len() };
        xai_grok_telemetry::unified_log::info(
            "shell.prompt.queued",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "prompt_id": prompt_id,
                "queue_depth": queue_depth,
            })),
        );

        // Log prompt to per-CWD fast history file immediately when queued
        // (not in handle_prompt, because prompt might be cancelled before processing)
        // Extract raw text from prompt_blocks (without <user_query> tags)
        let raw_prompt_text: String = prompt_blocks
            .iter()
            .filter_map(|block| {
                if let acp::ContentBlock::Text(t) = block {
                    Some(t.text.trim())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        let origin = crate::session::PromptOrigin::from_prompt_id(&prompt_id);

        // Bump before any await so a LocalSet recap cannot commit/emit after
        // this Prompt was accepted but before handle_prompt runs.
        if !origin.is_synthetic() {
            self.cancel_pending_recap_for_new_prompt();
        }

        // Don't write synthetic auto-wake prompts to prompt history.
        if !origin.is_synthetic() && !raw_prompt_text.is_empty() && !self.startup_hints.is_subagent
        {
            let cwd = self.session_info.cwd.clone();
            let session_id = self.session_info.id.to_string();
            let is_bash = Self::extract_bash_command(&prompt_blocks).is_some();
            // Await inline so the append is durable before quit drops the agent runtime's detached tasks.
            let entry = crate::session::prompt_history::PromptEntry {
                timestamp: chrono::Utc::now(),
                session_id,
                prompt: raw_prompt_text,
                is_bash,
            };
            crate::session::prompt_history::append_prompt_async(cwd, entry).await;
        }

        // Capture trace config template from the first real user prompt so
        // synthetic auto-wake turns can reuse the same GCS bucket/method.
        let (trace_gcs_config, artifact_tracker) = if let Some(ref cfg) = trace_gcs_config {
            *self.trace_config_template.borrow_mut() = Some(TraceConfigTemplate {
                bucket_url: cfg.bucket_url.clone(),
                upload_method: cfg.upload_method.clone(),
            });
            (trace_gcs_config, artifact_tracker)
        } else {
            (trace_gcs_config, artifact_tracker)
        };

        if let crate::session::PromptOrigin::SubagentCompleted { subagent_id } = &origin {
            self.mark_completions_reported(&[subagent_id]).await;
        }

        // For synthetic prompts, derive trace config from the template
        // captured during the first real user prompt.
        let (trace_gcs_config, artifact_tracker) =
            if origin.is_synthetic() && trace_gcs_config.is_none() {
                if let Some(template) = self.trace_config_template.borrow().clone() {
                    let cfg = crate::session::repo_changes::TraceExportConfig {
                        bucket_url: template.bucket_url,
                        service_account_key: None,
                        prefix_dir: None,
                        gcs_prefix: Some(format!(
                            "{}/turn_{}",
                            self.session_info.id.0,
                            self.chat_state_handle.get_prompt_index().await,
                        )),
                        absolute_paths: false,
                        archive_name_override: None,
                        upload_method: template.upload_method,
                    };
                    (
                        Some(cfg),
                        Some(crate::upload::manifest::new_artifact_tracker()),
                    )
                } else {
                    (None, None)
                }
            } else {
                (trace_gcs_config, artifact_tracker)
            };

        let mut state = self.state.lock().await;

        // User prompts have priority over queued synthetic auto-wake prompts;
        // the guarded sweep exempts the running turn's own slot (see
        // `State::sweep_pending_inputs`). Gate deliberately keyed on
        // completion-id-bearing synthetics only (pre-existing shape): a queue
        // holding only drain/goal-summary synthetics is never preempted.
        if !origin.is_synthetic() {
            let preempt_armed = state.pending_inputs.iter().any(|i| {
                i.origin.completion_id().is_some()
                    && state.running_prompt_id() != Some(i.prompt_id.as_str())
            });
            if preempt_armed {
                let dropped = state.sweep_pending_inputs(|i| i.origin.is_synthetic());
                if let Some(reservations) = &self.tool_context.task_completion_reservations {
                    for task_id in dropped
                        .iter()
                        .filter_map(|item| item.origin.completion_id())
                    {
                        reservations.release(task_id);
                    }
                }
                tracing::info!(
                    dropped_count = dropped.len(),
                    "auto-wake: dropping pending synthetic prompts (user prompt has priority)"
                );
            }
        }

        // Build the shared-queue metadata for user-originated prompts only.
        // Synthetic inputs (auto-wake, nudges, drains) are not user-visible
        // queue items.
        let queue_meta = if origin.is_synthetic() {
            None
        } else {
            // Derive the wire `kind` from the prompt content so the shared
            // queue / `running_prompt_id` adoption picks the right display
            // shim. Bash commands carry a bash
            // `PromptBlockMeta`; everything user-submitted here is otherwise a
            // plain prompt. (Cron prompts are server-injected via their own
            // path and render client-side.)
            let kind = if Self::extract_bash_command(&prompt_blocks).is_some() {
                "bash"
            } else {
                "prompt"
            };
            Some(crate::session::prompt_queue::QueueEntryMeta {
                id: prompt_id.clone(),
                version: 0,
                owner: client_identifier.clone(),
                last_editor: None,
                kind: kind.to_string(),
                text: Self::queue_text_from_blocks(&prompt_blocks),
                combined_texts: None,
            })
        };
        let log_prompt_id = prompt_id.clone();
        let log_kind = queue_meta
            .as_ref()
            .map(|m| m.kind.clone())
            .unwrap_or_else(|| "synthetic".to_string());
        let log_owner = client_identifier.clone().unwrap_or_default();
        let mut item = InputItem {
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
            task_wake_fallback,
            respond_to,
            persist_ack,
            parsed_prompt_tx,
            queue_meta,
            send_now: false,
        };

        // Use `running_prompt_id()` not `current_prompt_id` (cleared while front
        // unpopped). Auto send-now only if blocked wait + empty held queue.
        let running_front_id = state.running_prompt_id().map(str::to_string);
        let turn_running = running_front_id.is_some();
        let goal_active = self
            .tool_context
            .goal_loop_active_gate
            .load(std::sync::atomic::Ordering::Relaxed);
        let blocked_in_wait = self.tool_context.blocking_wait_depth.depth() > 0;
        let held_user_queue = state.pending_inputs.iter().any(|queued| {
            !queued.origin.is_synthetic()
                && Some(queued.prompt_id.as_str()) != running_front_id.as_deref()
        });
        let auto_send_now = turn_running && blocked_in_wait && !held_user_queue;
        let send_now = !item.origin.is_synthetic() && (send_now || auto_send_now);
        let cancel_running_turn = send_now && turn_running && !goal_active;
        if send_now {
            item.send_now = true;
            // Insert right behind the running front (never displace it —
            // `handle_completion` pops the front), else at the queue head —
            // but behind earlier send-now prompts still queued, so stacked
            // sends (e.g. during a goal turn, which never cancels) run FIFO.
            let mut insert_at = usize::from(matches!(
                (state.pending_inputs.front(), running_front_id.as_deref()),
                (Some(front_item), Some(running)) if front_item.prompt_id == running
            ));
            while state
                .pending_inputs
                .get(insert_at)
                .is_some_and(|queued| queued.send_now)
            {
                insert_at += 1;
            }
            state.pending_inputs.insert(insert_at, item);
        } else {
            state.pending_inputs.push_back(item);
        }
        // qtrace: server appended a prompt to the authoritative FIFO. The index
        // it lands at vs whether a turn is already running tells us if it will
        // run next or queue behind others (the leader-mode source of truth
        // that clients must mirror).
        tracing::debug!(
            target: "qtrace",
            pid = std::process::id(),
            event = "server_queue_input",
            prompt_id = %log_prompt_id,
            kind = %log_kind,
            owner = %log_owner,
            send_now,
            cancel_running_turn,
            new_depth = state.pending_inputs.len(),
            running_task_present = state.running_task.is_some(),
            session = self.session_info.id.0.as_ref(),
            "server appended prompt to pending_inputs",
        );
        if cancel_running_turn {
            xai_grok_telemetry::unified_log::info(
                "shell.prompt.send_now_cancels_turn",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({
                    "prompt_id": log_prompt_id,
                    "blocked_in_wait": blocked_in_wait,
                })),
            );
        }
        // Broadcast the new authoritative queue to all subscribers
        // (fire-and-forget, never persisted).
        self.broadcast_queue_changed(&state);
        cancel_running_turn
    }

    /// Extract a plain-text summary of a prompt's content blocks for the
    /// shared queue display.
    ///
    /// Prefers a block's `displayText` meta (the compact user-facing form, e.g.
    /// `/loop 5s echo "x"`) over the raw wire text. A client that expands a
    /// slash skill locally sends the full expanded instruction as the wire text
    /// with the compact invocation stamped in `displayText`; the shared queue —
    /// and the turn-start shim that renders other clients' user block from this
    /// text — must show the compact form, not the raw expansion. Falls back to
    /// the joined block text when no `displayText` is present.
    pub(super) fn queue_text_from_blocks(blocks: &[acp::ContentBlock]) -> String {
        if let Some(display) = blocks.iter().find_map(|block| {
            let acp::ContentBlock::Text(t) = block else {
                return None;
            };
            t.meta
                .as_ref()
                .and_then(|m| m.get("displayText"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        }) {
            return display;
        }
        blocks
            .iter()
            .filter_map(|block| {
                if let acp::ContentBlock::Text(t) = block {
                    Some(t.text.trim())
                } else {
                    None
                }
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Project the user-visible (queued, not-yet-running) prompts into the
    /// wire shape, in queue order. The currently-running prompt is excluded
    /// (it is shown via the normal turn stream, not the queue).
    pub(super) fn build_queue_wire(
        &self,
        state: &State,
    ) -> Vec<crate::session::prompt_queue::QueueEntryWire> {
        // Race-free running identity (`running_task` lives under the same lock
        // as `pending_inputs`); the `current_prompt_id` pin is cleared early by
        // `handle_completion`, which would briefly re-list the finished-but-
        // unpopped front as a queued row.
        let running_id = state.running_prompt_id();
        let mut out = Vec::new();
        for item in &state.pending_inputs {
            let Some(meta) = &item.queue_meta else {
                continue;
            };
            if running_id == Some(meta.id.as_str()) {
                // This item is the in-flight turn, not a queued prompt.
                continue;
            }
            out.push(crate::session::prompt_queue::QueueEntryWire {
                id: meta.id.clone(),
                version: meta.version,
                owner: meta.owner.clone(),
                last_editor: meta.last_editor.clone(),
                kind: meta.kind.clone(),
                text: meta.text.clone(),
                combined_texts: meta.combined_texts.clone(),
                position: out.len(),
            });
        }
        out
    }

    /// Broadcast the current authoritative prompt queue to all subscribers
    /// Fire-and-forget via the gateway, carrying `sessionId`
    /// so session routing fans it to every attached client. Never persisted.
    pub(super) fn broadcast_queue_changed(&self, state: &State) {
        let running = state.running_prompt_id().and_then(|pid| {
            state
                .pending_inputs
                .iter()
                .find(|i| i.prompt_id == pid)
                .map(Self::running_display_from_item)
        });
        self.broadcast_queue_changed_inner(state, running);
    }

    /// Broadcast with explicit running-turn display (promote before `running_task`
    /// so clients paint before the user-echo races in).
    pub(super) fn broadcast_queue_changed_promoting(
        &self,
        state: &State,
        running: RunningPromptDisplay,
    ) {
        self.broadcast_queue_changed_inner(state, Some(running));
    }

    pub(super) fn running_display_from_item(item: &InputItem) -> RunningPromptDisplay {
        let meta = item.queue_meta.as_ref();
        RunningPromptDisplay {
            id: item.prompt_id.clone(),
            text: meta
                .map(|m| m.text.clone())
                .unwrap_or_else(|| Self::queue_text_from_blocks(&item.prompt_blocks)),
            kind: meta
                .map(|m| m.kind.clone())
                .unwrap_or_else(|| "prompt".to_string()),
            combined_texts: meta
                .and_then(|m| m.combined_texts.clone())
                .filter(|v| v.len() >= 2),
        }
    }

    fn broadcast_queue_changed_inner(&self, state: &State, running: Option<RunningPromptDisplay>) {
        let running_id = running.as_ref().map(|r| r.id.clone());
        // Exclude the running/promoting row from `entries` (same as when
        // `running_task` is set).
        let mut entries = self.build_queue_wire(state);
        if let Some(rid) = running_id.as_deref() {
            entries.retain(|e| e.id != rid);
            for (i, e) in entries.iter_mut().enumerate() {
                e.position = i;
            }
        }
        let (running_text, running_kind, running_combined_texts) = match running {
            Some(r) => (Some(r.text), Some(r.kind), r.combined_texts),
            None => (None, None, None),
        };
        let payload = crate::session::prompt_queue::QueueChanged {
            session_id: self.session_info.id.0.to_string(),
            entries,
            running_prompt_id: running_id,
            running_text,
            running_kind,
            running_combined_texts,
        };
        tracing::debug!(
            target: "qtrace",
            pid = std::process::id(),
            event = "server_broadcast_queue",
            running_prompt_id = payload.running_prompt_id.as_deref().unwrap_or(""),
            combined_segs = payload
                .running_combined_texts
                .as_ref()
                .map(|v| v.len())
                .unwrap_or(0),
            entry_count = payload.entries.len(),
            entries = ?payload.entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            session = self.session_info.id.0.as_ref(),
            "broadcasting x.ai/queue/changed to subscribers",
        );
        if let Ok(params) = serde_json::value::to_raw_value(&payload) {
            self.notifications
                .gateway
                .forward_fire_and_forget(acp::ExtNotification::new(
                    crate::session::prompt_queue::QUEUE_CHANGED_METHOD,
                    params.into(),
                ));
        }
    }

    /// Whether `prompt_id` is the currently in-flight turn (and so must never
    /// be removed/reordered by a queue edit). Keyed on `state.running_task`
    /// (race-free under the caller's state lock), not the `current_prompt_id`
    /// pin, which `handle_completion` clears while the finished front is still
    /// unpopped — a queue edit in that window must still refuse the front.
    fn is_running_prompt(state: &State, prompt_id: &str) -> bool {
        state.running_prompt_id() == Some(prompt_id)
    }

    /// Remove a queued prompt by id. Versioned + idempotent:
    /// a missing id (already drained) or a stale `expected_version` is a
    /// benign no-op — the actor still re-broadcasts so the client reconciles.
    /// The in-flight turn is never removed. `owner` (when `Some`) scopes the
    /// edit to the requesting client's own items.
    /// Resolve a removed prompt's in-flight `session/prompt` RPC
    /// before its [`InputItem`] is dropped.
    ///
    /// A queued prompt still has a client awaiting its `respond_to` oneshot (the
    /// leader's `MvpAgent::prompt()` handler blocks on it). Dropping the sender
    /// unfulfilled makes that await fail with `RecvError`, which the handler
    /// turns into `acp::Error::internal_error("session failed to respond")`.
    /// Worse, the client's `PromptResponse` handler only applies its
    /// prompt-id gate on the `Ok` path, so that `Err` is misattributed to the
    /// *running* turn and rendered as a spurious "Turn failed" — the session
    /// appears to die.
    ///
    /// Report success with [`PromptCompletionKind::RemovedFromQueue`] instead:
    /// the response is now `Ok`, so the client's prompt-id gate sees it isn't
    /// the running turn and silently discards it, leaving the active turn
    /// untouched. Crucially, `RemovedFromQueue` makes the leader's `prompt()`
    /// handler short-circuit BEFORE the `prompt_complete` broadcast + roster
    /// delta, so other attached clients (leader mode) don't see the running
    /// turn spuriously end. Token count is `0` — a removed queued prompt never
    /// ran (and the value is discarded by the gate regardless).
    pub(super) fn respond_removed_prompt(respond_to: oneshot::Sender<PromptTurnResult>) {
        let _ = respond_to.send(Ok(PromptTurnOk {
            stop_reason: acp::StopReason::Cancelled,
            total_tokens: 0,
            turn_snapshot: None,
            completion_kind: PromptCompletionKind::RemovedFromQueue,
            structured_output: None,
            usage: None,
        }));
    }

    pub(super) async fn handle_remove_queued_prompt(
        &self,
        id: &str,
        expected_version: u64,
        owner: Option<&str>,
    ) {
        let mut state = self.state.lock().await;
        let mut removed = false;
        if !Self::is_running_prompt(&state, id)
            && let Some(pos) = state.pending_inputs.iter().position(|item| {
                item.queue_meta.as_ref().is_some_and(|m| {
                    m.id == id
                        && m.version == expected_version
                        && owner.is_none_or(|o| m.owner.as_deref() == Some(o))
                })
            })
        {
            if let Some(item) = state.pending_inputs.remove(pos) {
                Self::respond_removed_prompt(item.respond_to);
            }
            removed = true;
        }
        if !removed {
            tracing::debug!(
                queued_id = %id,
                expected_version,
                "queue remove was a no-op (drained / stale / not owner); rebroadcasting"
            );
        }
        // Always re-broadcast the authoritative queue so the client reconciles.
        self.broadcast_queue_changed(&state);
    }

    /// Atomically interject a queued (not-yet-running) prompt into the running
    /// turn. In a single state-lock hold the actor removes the
    /// prompt from `pending_inputs` and pushes its text into
    /// `pending_interjections`, so the in-flight turn merges it at the next safe
    /// point (`drain_pending_interjections`) and the prompt can never both
    /// interject AND later run as its own turn — the race the client-side
    /// "interject + queue/remove" pair could not avoid.
    ///
    /// Mirrors [`handle_remove_queued_prompt`]'s versioned/owner gate and
    /// [`SessionCommand::Interject`]'s broadcast-then-buffer. Benign **no-op**
    /// (the prompt stays queued and runs normally) when:
    /// - no turn is running (the `Send now` race where the turn just ended —
    ///   buffering into nothing would strand the text), or
    /// - `id` names the running turn, is already drained/removed, carries a
    ///   stale `expected_version`, or is owned by another client, or
    /// - the row is not a plain prompt (it would reach the model as prompt text).
    ///
    /// Always re-broadcasts `x.ai/queue/changed` so every client reconciles
    /// (the row vanishes on success, is unchanged on a no-op).
    /// `new_text` (when `Some`) replaces the stored queue text in the
    /// interjection — the client edited the row before interjecting. It rides
    /// the same version check, so a stale version no-ops the edit too.
    /// Exception: when the interject no-ops but the row is still queued, a
    /// version-matching `new_text` is saved to the row as an LWW edit so the
    /// edit isn't silently lost when the row later drains as its own turn.
    pub(super) async fn handle_interject_queued_prompt(
        &self,
        id: &str,
        expected_version: u64,
        owner: Option<&str>,
        new_text: Option<&str>,
    ) -> bool {
        let mut state = self.state.lock().await;
        let running_front_id = state.running_prompt_id().map(str::to_string);
        let turn_running = running_front_id.is_some();
        let goal_active = self
            .tool_context
            .goal_loop_active_gate
            .load(std::sync::atomic::Ordering::Relaxed);
        let row_matches = |item: &InputItem| {
            item.queue_meta.as_ref().is_some_and(|m| {
                m.id == id
                    && m.version == expected_version
                    && owner.is_none_or(|o| m.owner.as_deref() == Some(o))
            })
        };
        let running_is_row = running_front_id.as_deref() == Some(id);
        let pos = if running_is_row {
            None
        } else {
            state.pending_inputs.iter().position(row_matches)
        };
        let mut cancel_running_turn = false;
        if let Some(pos) = pos
            && let Some(mut item) = state.pending_inputs.remove(pos)
        {
            // Client-edited text wins (LWW).
            if let Some(new_text) = new_text.filter(|t| !t.trim().is_empty()) {
                Self::apply_queued_prompt_edit(&mut item, new_text.to_string(), owner);
            }
            // Send-now: promote the row to run as the next turn, not an
            // interjection. Land behind earlier send-now prompts still queued
            // (FIFO among sends), mirroring `queue_input`.
            item.send_now = true;
            let mut insert_at = usize::from(matches!(
                (state.pending_inputs.front(), running_front_id.as_deref()),
                (Some(front_item), Some(running)) if front_item.prompt_id == running
            ));
            while state
                .pending_inputs
                .get(insert_at)
                .is_some_and(|queued| queued.send_now)
            {
                insert_at += 1;
            }
            state.pending_inputs.insert(insert_at, item);
            cancel_running_turn = turn_running && !goal_active;
            xai_grok_telemetry::unified_log::info(
                "shell.prompt.send_now_cancels_turn",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({
                    "prompt_id": id,
                    "from_queue_row": true,
                    "cancels_turn": cancel_running_turn,
                    "goal_active": goal_active,
                })),
            );
            tracing::info!(queued_id = %id, cancel_running_turn, "send-now: promoted queued prompt to run next");
        } else if let Some(new_text) = new_text
            && !new_text.trim().is_empty()
            && !running_is_row
            && let Some(item) = state
                .pending_inputs
                .iter_mut()
                .find(|item| row_matches(item))
        {
            // The send-now no-opped but the row is still queued: keep the
            // edit as an LWW write so it isn't silently lost. Stale versions
            // get no fallback (LWW); the running turn is never edited.
            Self::apply_queued_prompt_edit(item, new_text.to_string(), owner);
            tracing::info!(
                queued_id = %id,
                "send-now no-opped; saved the edit to the queued row"
            );
        } else {
            tracing::debug!(
                queued_id = %id,
                expected_version,
                turn_running,
                "queue send-now no-op (running id / stale / drained / not owner); rebroadcasting"
            );
        }
        // Always re-broadcast the authoritative queue so the client reconciles.
        self.broadcast_queue_changed(&state);
        cancel_running_turn
    }

    /// Reorder queued prompts to match `ordered_ids`. The
    /// running turn (front, if active) stays pinned at the front; queued items
    /// not named in `ordered_ids` keep their relative order behind the named
    /// ones. Idempotent; re-broadcasts the result.
    pub(super) async fn handle_reorder_queue(&self, ordered_ids: &[String]) {
        let mut state = self.state.lock().await;

        // Partition: items we never reorder (running turn front + synthetic /
        // non-queue items) vs reorderable queued user prompts.
        let running_id = state.running_prompt_id().map(str::to_string);
        let mut pinned: std::collections::VecDeque<InputItem> = std::collections::VecDeque::new();
        let mut queued: Vec<InputItem> = Vec::new();
        for item in std::mem::take(&mut state.pending_inputs) {
            let is_queueable = item
                .queue_meta
                .as_ref()
                .is_some_and(|m| running_id.as_deref() != Some(m.id.as_str()));
            if is_queueable {
                queued.push(item);
            } else {
                pinned.push_back(item);
            }
        }

        // Stable reorder: named ids first (in requested order), then the rest.
        let rank = |item: &InputItem| -> usize {
            item.queue_meta
                .as_ref()
                .and_then(|m| ordered_ids.iter().position(|x| x == &m.id))
                .unwrap_or(usize::MAX)
        };
        queued.sort_by_key(rank);

        let mut rebuilt = pinned;
        rebuilt.extend(queued);
        state.pending_inputs = rebuilt;

        self.broadcast_queue_changed(&state);
    }

    /// Clear queued prompts. When `owner` is `Some`, only that
    /// client's queued items are removed. The running turn is never touched.
    pub(super) async fn handle_clear_queue(&self, owner: Option<&str>) {
        let mut state = self.state.lock().await;
        // Partition rather than `retain`: each cleared user prompt still has a
        // client awaiting its `respond_to`, so it must be resolved with
        // `Cancelled` (see [`respond_removed_prompt`]) instead of being
        // dropped — a bare drop surfaces as "session failed to respond" and a
        // spurious "Turn failed" on the running turn.
        let running_id = state.running_prompt_id().map(str::to_string);
        let mut kept = VecDeque::with_capacity(state.pending_inputs.len());
        for item in std::mem::take(&mut state.pending_inputs) {
            let keep = match &item.queue_meta {
                // Non-queue (synthetic) items always stay.
                None => true,
                // Never drop the in-flight turn; keep items NOT owned by the
                // requester (owner-scoped clear).
                Some(meta) => {
                    running_id.as_deref() == Some(meta.id.as_str())
                        || owner.is_some_and(|o| meta.owner.as_deref() != Some(o))
                }
            };
            if keep {
                kept.push_back(item);
            } else {
                Self::respond_removed_prompt(item.respond_to);
            }
        }
        state.pending_inputs = kept;
        self.broadcast_queue_changed(&state);
    }

    /// Replace the text of a queued (not-yet-running) prompt in place
    /// (LWW).
    ///
    /// Semantics — last write wins via the actor's serialized mailbox.
    /// Concretely, for an entry whose `queue_meta.id == id`:
    /// 1. Rebuild the underlying `prompt_blocks` as a single
    ///    [`acp::TextContent`] block carrying `new_text` (any non-text blocks
    ///    such as pasted images on the original prompt are not preserved — the
    ///    user has explicitly typed replacement text).
    /// 2. Update `queue_meta.text`, bump `queue_meta.version`, and record
    ///    `last_editor` (the original `owner` attribution is preserved).
    /// 3. Re-broadcast `x.ai/queue/changed` so every subscriber renders the
    ///    new text and version.
    ///
    /// **No-op cases** (each is a benign discard with no rebroadcast — nothing
    /// changed):
    /// - The id is not in `pending_inputs` (already drained / removed).
    /// - The id names the currently-running turn — editing the live turn is
    ///   out of scope.
    /// - `new_text` is blank (a queued prompt is never blanked).
    pub(super) async fn handle_edit_queued_prompt(
        &self,
        id: &str,
        new_text: String,
        editor: Option<&str>,
    ) {
        if new_text.trim().is_empty() {
            tracing::debug!(queued_id = %id, "queue edit no-op: empty newText");
            return;
        }
        let mut state = self.state.lock().await;
        // Locked first: the promoter arms `running_task` under this lock.
        if Self::is_running_prompt(&state, id) {
            tracing::debug!(
                queued_id = %id,
                "queue edit no-op: id names the running turn"
            );
            return;
        }
        let Some(item) = state
            .pending_inputs
            .iter_mut()
            .find(|item| item.queue_meta.as_ref().is_some_and(|m| m.id == id))
        else {
            tracing::debug!(
                queued_id = %id,
                "queue edit no-op: id not found (already drained / removed)"
            );
            return;
        };
        Self::apply_queued_prompt_edit(item, new_text, editor);
        self.broadcast_queue_changed(&state);
    }

    /// Merge consecutive plain prompts into `pending[0]` via
    /// [`xai_prompt_queue::combine_prefix_len`]. `skip_ids` holds rows under
    /// composer edit. Merged-away items complete as
    /// [`PromptCompletionKind::RemovedFromQueue`].
    pub(super) fn combine_front_pending_inputs(
        pending: &mut std::collections::VecDeque<InputItem>,
        skip_ids: &[&str],
    ) {
        use xai_prompt_queue::{CombineGate, combine_prefix_len};

        if pending.len() < 2 {
            return;
        }
        let gates: Vec<CombineGate<'_>> = pending.iter().map(Self::combine_gate).collect();
        let n = combine_prefix_len(gates, skip_ids);
        if n < 2 {
            return;
        }
        for _ in 1..n {
            let Some(next) = pending.remove(1) else {
                break;
            };
            // The follower's text is folded into the front's turn below, so it
            // still runs — but its own queue row is gone, so it resolves as
            // RemovedFromQueue (the same completion a client sees for an
            // explicit dequeue). The multi-client UI repaints its bubble from
            // the promote broadcast's `running_combined_texts`.
            Self::respond_removed_prompt(next.respond_to);
            let extra = Self::joined_text_blocks(&next.prompt_blocks);
            if let Some(front) = pending.front_mut() {
                Self::append_text_to_prompt(front, &extra);
            }
        }
    }

    fn combine_gate(item: &InputItem) -> xai_prompt_queue::CombineGate<'_> {
        let is_bash = Self::extract_bash_command(&item.prompt_blocks).is_some();
        let is_plain_prompt =
            item.queue_meta.as_ref().map(|m| m.kind.as_str()) == Some("prompt") && !is_bash;
        let mut has_text = false;
        let mut has_images = false;
        let mut is_expanded_skill = false;
        let mut non_text_non_image = false;
        for block in &item.prompt_blocks {
            match block {
                acp::ContentBlock::Text(t) => {
                    if Self::has_display_text(t) {
                        is_expanded_skill = true;
                    }
                    if !t.text.is_empty() {
                        has_text = true;
                    }
                }
                acp::ContentBlock::Image(_) => has_images = true,
                _ => non_text_non_image = true,
            }
        }
        // Follower eligibility also requires single plain text; encode via
        // is_expanded_skill / has_images / non_text_non_image.
        let text = item
            .queue_meta
            .as_ref()
            .map(|m| m.text.as_str())
            .unwrap_or("");
        xai_prompt_queue::CombineGate {
            id: item.prompt_id.as_str(),
            is_plain_prompt: is_plain_prompt && has_text && !non_text_non_image,
            is_synthetic: item.origin.is_synthetic(),
            is_expanded_skill,
            is_bash,
            has_images,
            text: if text.is_empty() {
                // Fall back so empty meta still participates when blocks have text.
                item.prompt_blocks
                    .iter()
                    .find_map(|b| match b {
                        acp::ContentBlock::Text(t) if !t.text.is_empty() => Some(t.text.as_str()),
                        _ => None,
                    })
                    .unwrap_or("")
            } else {
                text
            },
        }
    }

    fn has_display_text(t: &acp::TextContent) -> bool {
        t.meta
            .as_ref()
            .and_then(|m| m.get("displayText"))
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.trim().is_empty())
    }

    fn joined_text_blocks(blocks: &[acp::ContentBlock]) -> String {
        use xai_prompt_queue::join_texts;
        join_texts(blocks.iter().filter_map(|block| match block {
            acp::ContentBlock::Text(t) if !t.text.is_empty() => Some(t.text.as_str()),
            _ => None,
        }))
    }

    fn append_text_to_prompt(item: &mut InputItem, extra: &str) {
        use xai_prompt_queue::TEXT_SEPARATOR;

        if extra.is_empty() {
            return;
        }
        if let Some(meta) = item.queue_meta.as_mut() {
            match meta.combined_texts.as_mut() {
                Some(segs) => segs.push(extra.to_string()),
                None => {
                    meta.combined_texts = Some(vec![meta.text.clone(), extra.to_string()]);
                }
            }
        }
        // Append to the LAST text block so a multi-text front stays ordered
        // (front text first, then the follower); `combined_texts` mirrors that.
        if let Some(acp::ContentBlock::Text(t)) = item
            .prompt_blocks
            .iter_mut()
            .rev()
            .find(|b| matches!(b, acp::ContentBlock::Text(_)))
        {
            if !t.text.is_empty() {
                t.text.push_str(TEXT_SEPARATOR);
            }
            t.text.push_str(extra);
        }
        if let Some(meta) = item.queue_meta.as_mut() {
            meta.text = Self::queue_text_from_blocks(&item.prompt_blocks);
        }
        Self::stamp_combined_display_texts_meta(item);
    }

    fn stamp_combined_display_texts_meta(item: &mut InputItem) {
        use xai_prompt_queue::stamp_combined_display_texts;

        let Some(segs) = item
            .queue_meta
            .as_ref()
            .and_then(|m| m.combined_texts.as_ref())
            .cloned()
        else {
            return;
        };
        // Stamp the first text block (matches append_text_to_prompt); an
        // image-first front would otherwise lose the replay multi-bubble meta.
        let Some(acp::ContentBlock::Text(t)) = item
            .prompt_blocks
            .iter_mut()
            .find(|b| matches!(b, acp::ContentBlock::Text(_)))
        else {
            return;
        };
        let map = t.meta.get_or_insert_with(acp::Meta::new);
        stamp_combined_display_texts(map, &segs);
    }

    /// Replace a queued item's prompt body with `new_text` and bump its LWW
    /// version metadata. Shared by `handle_edit_queued_prompt` and the
    /// turn-ended fallback in `handle_interject_queued_prompt`.
    ///
    /// Replaces the text blocks with a single text block carrying the new
    /// text; Image blocks are RETAINED — the queue-edit wire is text-only,
    /// so a text edit must never silently detach the row's pasted images
    /// (mirrors the pager's local-row edit semantics). Other non-text
    /// blocks are still dropped — an explicit retype is a fresh prompt
    /// body. The `displayText` meta is left unset so the queue text shown
    /// to other clients is exactly what the editor typed (no stale skill
    /// expansion).
    fn apply_queued_prompt_edit(item: &mut InputItem, new_text: String, editor: Option<&str>) {
        // A bash row executes `extract_bash_command`'s meta value, not the
        // block text — rebuild the meta with the edited text or the edit
        // demotes the row to a plain model prompt.
        let meta = Self::extract_bash_command(&item.prompt_blocks)
            .is_some()
            .then(|| {
                let value = serde_json::to_value(
                    crate::extensions::prompt_meta::PromptBlockMeta::bash(new_text.clone()),
                )
                .expect("PromptBlockMeta serializes");
                value
                    .as_object()
                    .cloned()
                    .expect("PromptBlockMeta serializes to object")
            });
        let mut blocks = vec![acp::ContentBlock::Text(
            acp::TextContent::new(new_text.clone()).meta(meta),
        )];
        blocks.extend(
            std::mem::take(&mut item.prompt_blocks)
                .into_iter()
                .filter(|b| matches!(b, acp::ContentBlock::Image(_))),
        );
        item.prompt_blocks = blocks;
        if let Some(meta) = item.queue_meta.as_mut() {
            meta.text = new_text;
            meta.combined_texts = None;
            meta.version = meta.version.saturating_add(1);
            meta.last_editor = editor.map(str::to_string);
        }
    }
}
