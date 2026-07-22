//! Memory concern for `SessionActor`: memory flush, the dream pipeline,
//! memory tool registration, and note rewriting.

use super::*;

#[derive(Debug)]
pub(super) struct MemoryFlushSnapshot {
    counts: xai_chat_state::ConversationCounts,
    chat_history: Vec<ChatRequestMessage>,
}

/// Build first-turn injection backend params without mutating the shared
/// session params.
///
/// This clones the session-wide backend params so tool-search and
/// compaction-recovery backends keep their original `search_source` and search
/// thresholds. The returned effective min score preserves the historical
/// first-turn default of `0.0` unless the injection config explicitly
/// overrides it.
pub(super) fn build_initial_injection_backend_params(
    params: &crate::session::memory::MemoryBackendParams,
    initial_injection_config: &crate::config::MemoryInitialInjectionConfig,
) -> (crate::session::memory::MemoryBackendParams, f64) {
    let mut injection_params = params.clone();
    injection_params.search_source = "injection";
    let effective_min_score = initial_injection_config
        .min_score
        .map(|min_score| {
            injection_params.search_config.min_score = min_score;
            min_score as f64
        })
        .unwrap_or(0.0);
    (injection_params, effective_min_score)
}

impl SessionActor {
    /// Re-register `memory_search` and `memory_get` tools on the tool bridge.
    ///
    /// Used when re-enabling memory mid-session (`/memory on`). The tools are
    /// registered via the dynamic `register_mcp_tools` path which puts them in
    /// the `LocalRegistry` for dispatch. The memory backend itself is already
    /// in `Resources` (inserted by the caller before calling this method).
    pub(super) async fn register_memory_tools(
        &self,
        bridge: &xai_grok_tools::bridge::ToolBridge,
    ) -> Result<(), String> {
        use xai_grok_tools::implementations::memory::{
            MEMORY_GET_TOOL_NAME, MEMORY_SEARCH_TOOL_NAME,
        };

        bridge
            .register_mcp_tools(
                MEMORY_SEARCH_TOOL_NAME.to_owned(),
                xai_grok_tools::implementations::memory::search_tool::MemorySearchImpl,
                None,
            )
            .await
            .map_err(|e| format!("failed to register memory_search: {e}"))?;
        bridge
            .register_mcp_tools(
                MEMORY_GET_TOOL_NAME.to_owned(),
                xai_grok_tools::implementations::memory::get_tool::MemoryGetImpl,
                None,
            )
            .await
            .map_err(|e| format!("failed to register memory_get: {e}"))?;
        Ok(())
    }

    pub(super) fn emit_memory_session_summary(
        &self,
        telem: &super::memory_state::MemoryTelemetry,
        total_chunks_at_end: usize,
        session_end_result: &str,
    ) {
        xai_grok_telemetry::session_ctx::log_event(
            xai_grok_telemetry::memory_telemetry::MemorySessionSummary {
                session_id: self.session_info.id.to_string(),
                session_duration_secs: self.session_start.elapsed().as_secs(),
                flush_count: telem.flush_count,
                flush_success_count: telem.flush_success_count,
                flush_error_count: telem.flush_error_count,
                tool_search_count: telem.tool_search_count,
                injection_count: telem.injection_count,
                recovery_search_count: telem.compaction_recovery_count,
                total_chunks_at_end,
                chunks_added_this_session: telem.chunks_added as usize,
                session_end_result: session_end_result.to_owned(),
                dream_count: telem.dream_count,
                dream_success_count: telem.dream_success_count,
                dream_error_count: telem.dream_error_count,
            },
        );
    }

    /// Reindex a single file and embed any new chunks.
    ///
    /// Used after flush writes and session-end writes to keep the index
    /// and embeddings up to date immediately.
    pub(super) async fn reindex_and_embed(&self, path: &std::path::Path, source: &str) {
        self.memory.reindex_and_embed(path, source).await;
    }

    /// Common setup for dream methods: storage, lock, sessions dir, and truncated session id.
    fn dream_context(
        &self,
    ) -> Option<(
        crate::session::memory::MemoryStorage,
        crate::session::memory::dream_lock::DreamLock,
        std::path::PathBuf,
        String,
    )> {
        let storage = self.memory.storage()?;
        let workspace_dir = storage.workspace_dir();
        let lock = crate::session::memory::dream_lock::DreamLock::new(workspace_dir);
        let sessions_dir = storage.sessions_dir();
        let sid = &self.session_info.id.0;
        let sid8 = sid[..8.min(sid.len())].to_owned();
        Some((storage, lock, sessions_dir, sid8))
    }

    /// Run dream consolidation if gates pass.
    ///
    /// Called at session end after the session summary is written.
    /// Uses the same sampling client infrastructure as flush but sends
    /// the dream prompt instead. The model call has a 60s timeout.
    pub(super) async fn maybe_run_dream(&self) {
        if self.startup_hints.is_subagent {
            tracing::debug!(
                target: xai_grok_telemetry::memory_log::TARGET,
                "MEMORY_SUBAGENT_SKIP: skipping dream for subagent session"
            );
            return;
        }

        use crate::session::memory::dream::*;

        let Some((storage, lock, sessions_dir, sid8)) = self.dream_context() else {
            return;
        };

        let gate = check_dream_gates(&self.memory.dream_config, &lock, &sessions_dir, Some(&sid8));
        let sessions = match gate {
            DreamGate::Open { sessions } => sessions,
            other => {
                tracing::info!(
                    target: xai_grok_telemetry::memory_log::TARGET,
                    gate = ?other,
                    "MEMORY_DREAM: gate check result, skipping"
                );
                return;
            }
        };

        tracing::info!(
            target: xai_grok_telemetry::memory_log::TARGET,
            session_count = sessions.len(),
            "MEMORY_DREAM: gates passed, starting consolidation"
        );

        self.run_dream_inner(&storage, &lock, &sessions_dir, &sessions, "MEMORY_DREAM")
            .await;
    }

    /// Run dream from `/dream` slash command, bypassing time/session gates.
    pub(super) async fn run_dream_slash_command(&self) {
        use crate::session::memory::dream_lock::sessions_since;

        let Some((storage, lock, sessions_dir, sid8)) = self.dream_context() else {
            return;
        };

        let sessions = match sessions_since(
            &sessions_dir,
            std::time::SystemTime::UNIX_EPOCH,
            Some(&sid8),
        ) {
            Ok(s) if s.is_empty() => {
                tracing::info!(
                    target: xai_grok_telemetry::memory_log::TARGET,
                    "MEMORY_DREAM_SLASH: no session logs found, nothing to consolidate"
                );
                return;
            }
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    target: xai_grok_telemetry::memory_log::TARGET,
                    error = %e,
                    "MEMORY_DREAM_SLASH: failed to list sessions"
                );
                return;
            }
        };

        tracing::info!(
            target: xai_grok_telemetry::memory_log::TARGET,
            session_count = sessions.len(),
            "MEMORY_DREAM_SLASH: starting manual consolidation"
        );

        self.run_dream_inner(
            &storage,
            &lock,
            &sessions_dir,
            &sessions,
            "MEMORY_DREAM_SLASH",
        )
        .await;
    }

    /// Shared dream execution: build message, call model, execute, record result.
    async fn run_dream_inner(
        &self,
        storage: &crate::session::memory::MemoryStorage,
        lock: &crate::session::memory::dream_lock::DreamLock,
        sessions_dir: &std::path::Path,
        sessions: &[String],
        log_prefix: &str,
    ) {
        use crate::session::memory::dream::*;

        let existing_memory = std::fs::read_to_string(storage.workspace_memory_file()).ok();

        let dream_msg =
            match build_dream_user_message(sessions_dir, sessions, existing_memory.as_deref()) {
                Some(msg) => msg,
                None => {
                    tracing::info!(
                        target: xai_grok_telemetry::memory_log::TARGET,
                        "{log_prefix}: no readable session content, skipping"
                    );
                    return;
                }
            };

        let model_response = match tokio::time::timeout(
            std::time::Duration::from_secs(30 * 60),
            self.run_dream_model_call(&dream_msg.content),
        )
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                tracing::warn!(
                    target: xai_grok_telemetry::memory_log::TARGET,
                    error = %e,
                    "{log_prefix}: model call failed"
                );
                self.memory.record_dream_result(false);
                return;
            }
            Err(_) => {
                tracing::warn!(
                    target: xai_grok_telemetry::memory_log::TARGET,
                    "{log_prefix}: model call timed out (30m)"
                );
                self.memory.record_dream_result(false);
                return;
            }
        };

        let result = execute_dream(
            lock,
            storage,
            &model_response,
            sessions.len(),
            self.memory.dream_config.stale_lock_secs,
            sessions_dir,
            &dream_msg.processed_stems,
        );

        match &result.status {
            DreamStatus::Completed { .. } => self.memory.record_dream_result(true),
            DreamStatus::Failed(_) => self.memory.record_dream_result(false),
            _ => self.memory.record_dream_neutral(),
        }

        let dream_path = if matches!(result.status, DreamStatus::Completed { .. }) {
            let path = storage.workspace_memory_file();
            self.memory.reindex_and_embed(&path, "dream").await;

            // Remove stale index chunks only for session files that
            // were actually deleted — stems skipped by the recency guard
            // are still on disk and must remain searchable.
            if !result.cleaned_stems.is_empty() {
                let deleted_paths: Vec<std::path::PathBuf> = result
                    .cleaned_stems
                    .iter()
                    .map(|stem| sessions_dir.join(format!("{stem}.md")))
                    .collect();
                self.memory.delete_paths_from_index(&deleted_paths);
            }

            Some(path.display().to_string())
        } else {
            None
        };

        let dream_result_str = match &result.status {
            DreamStatus::Completed { chars_written } => format!("written ({chars_written} chars)"),
            DreamStatus::NothingToConsolidate => "nothing to consolidate".into(),
            DreamStatus::Skipped(reason) => format!("skipped: {reason}"),
            DreamStatus::Failed(err) => format!("failed: {err}"),
        };
        self.send_xai_notification(XaiSessionUpdate::MemoryDreamCompleted {
            result: dream_result_str,
            path: dream_path,
        })
        .await;

        tracing::info!(
            target: xai_grok_telemetry::memory_log::TARGET,
            status = ?result.status,
            sessions_eligible = result.sessions_eligible,
            sessions_cleaned = result.cleaned_stems.len(),
            "{log_prefix}: consolidation complete"
        );
    }

    /// Make the dream model call using the session's sampling client.
    async fn run_dream_model_call(&self, user_message: &str) -> Result<String, acp::Error> {
        let sampling_client = self.prepare_chat_completion(false).await?;
        let model = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| c.model)
            .unwrap_or_default();
        let session_id = self.session_info.id.to_string();
        let request = ConversationRequest {
            items: vec![
                ConversationItem::system(crate::session::memory::dream::DREAM_SYSTEM_PROMPT),
                ConversationItem::user(user_message),
            ],
            model: Some(model),
            x_grok_conv_id: Some(format!("dream-{}", uuid::Uuid::new_v4())),
            x_grok_req_id: Some(format!("xai-dream-{}", uuid::Uuid::new_v4())),
            x_grok_session_id: Some(session_id),
            x_grok_agent_id: Some(xai_grok_telemetry::id::agent_id()),
            ..Default::default()
        };
        let response = sampling_client
            .conversation_collect(request)
            .await
            .map_err(|e| {
                acp::Error::internal_error().data(format!("dream model call failed: {e}"))
            })?;
        Ok(response.assistant_text())
    }

    /// Run a memory flush turn that summarizes recent conversation into a
    /// session log. Sets `is_flushing` to suppress auto-compact during the call.
    ///
    /// Flush failure is non-fatal; compaction proceeds regardless.
    ///
    /// Returns `true` if a flush was executed, `false` if skipped because
    /// another flush is already in progress.
    pub(super) async fn run_memory_flush(
        &self,
        trigger: &str,
        snapshot: Option<MemoryFlushSnapshot>,
    ) -> bool {
        use crate::session::helpers::memory_flush::*;

        // Atomically acquire the flushing lock. If another flush is already
        // running (idle timer, pre-compaction, or user-requested), skip.
        if !self.memory.try_acquire_flush_lock() {
            tracing::info!(
                target: xai_grok_telemetry::memory_log::TARGET,
                "MEMORY_FLUSH: skipped — another flush is already in progress (trigger={trigger})"
            );
            return false;
        }

        tracing::info!(target: xai_grok_telemetry::memory_log::TARGET, "MEMORY_FLUSH: starting");
        let flush_start = std::time::Instant::now();

        self.send_xai_notification(XaiSessionUpdate::MemoryFlushStarted)
            .await;

        let result = async {
            let sampling_client = self.prepare_chat_completion(false).await?;
            let MemoryFlushSnapshot {
                counts,
                chat_history,
            } = match snapshot {
                Some(snapshot) => snapshot,
                None => self.snapshot_memory_flush_state().await,
            };
            xai_grok_telemetry::session_ctx::log_event(
                xai_grok_telemetry::memory_telemetry::MemoryFlushStart {
                    session_id: self.session_info.id.to_string(),
                    trigger: trigger.to_owned(),
                    conversation_len: counts.total,
                    user_message_count: counts.user,
                },
            );
            tracing::info!(
                target: xai_grok_telemetry::memory_log::TARGET,
                "MEMORY_FLUSH: conversation has {user} user, {assistant} assistant, {tool} tool messages ({total} total)",
                user = counts.user,
                assistant = counts.assistant,
                tool = counts.tool_result,
                total = counts.total,
            );
            let recent = super::helpers::memory_flush::select_flush_window(chat_history, 20);

            let flush_count = self.memory.flush_count.load(std::sync::atomic::Ordering::Relaxed);
            let system_prompt = if flush_count > 0 {
                if let Some(prev) = self.memory.last_flush_content.borrow().as_deref() {
                    format!("{FLUSH_DELTA_SYSTEM_PROMPT}{prev}")
                } else {
                    FLUSH_SYSTEM_PROMPT.to_owned()
                }
            } else {
                FLUSH_SYSTEM_PROMPT.to_owned()
            };
            let mut items: Vec<ConversationItem> = vec![ConversationItem::system(system_prompt)];
            tracing::info!(
                target: xai_grok_telemetry::memory_log::TARGET,
                "MEMORY_FLUSH: sending {n} recent messages to model (+ system prompt + user closer)",
                n = recent.len(),
            );
            items.extend(recent.into_iter().map(ConversationItem::from));
            items.push(ConversationItem::user(
                "Now write the memory summary as described in the system prompt.",
            ));

            let model = match self.memory.flush_config.flush_model.clone() {
                Some(m) => m,
                None => self.chat_state_handle.get_sampling_config().await
                    .map(|c| c.model)
                    .unwrap_or_default(),
            };
            tracing::info!(
                target: xai_grok_telemetry::memory_log::TARGET,
                "MEMORY_FLUSH: using model={model}"
            );
            let session_id = self.session_info.id.to_string();
            let request = ConversationRequest {
                items,
                model: Some(model),
                x_grok_conv_id: Some(format!("flush-{}", uuid::Uuid::new_v4())),
                x_grok_req_id: Some(format!("xai-flush-{}", uuid::Uuid::new_v4())),
                x_grok_session_id: Some(session_id.clone()),
                x_grok_agent_id: Some(xai_grok_telemetry::id::agent_id()),
                ..Default::default()
            };

            // Run on the multi-threaded runtime so it doesn't block the
            // session's LocalSet.
            let handle = tokio::spawn(async move {
                let response = sampling_client
                    .conversation_collect(request)
                    .await
                    .map_err(|e| format!("flush model call failed: {e}"))?;
                Ok::<_, String>(response.assistant_text())
            });
            // Abort the spawned task if this future is dropped (session
            // cancellation), preventing orphan HTTP streams.
            struct AbortOnDrop(tokio::task::AbortHandle);
            impl Drop for AbortOnDrop {
                fn drop(&mut self) {
                    self.0.abort();
                }
            }
            let _guard = AbortOnDrop(handle.abort_handle());
            handle
                .await
                .map_err(|e| {
                    acp::Error::internal_error()
                        .data(format!("flush stream task panicked: {e}"))
                })?
                .map_err(|e| acp::Error::internal_error().data(e))
        }
        .await;

        // (outcome_string, response_length, accepted_length, was_truncated, written_path)
        let (outcome, response_len, accepted_len, was_truncated, flush_path) = match result {
            Ok(response_text) => {
                let resp_len = response_text.len();
                match process_flush_response(&response_text, &self.memory.flush_config) {
                    FlushResult::NothingToStore => {
                        tracing::debug!("memory flush: nothing to store");
                        ("nothing to store".to_string(), resp_len, 0, false, None)
                    }
                    FlushResult::Accepted(content) => {
                        let acc_len = content.len();
                        let truncated = acc_len < resp_len;

                        // Semantic dedup: check if this content overlaps with
                        // existing memory chunks before writing.
                        let is_sem_dup = if let Some(storage) = self.memory.storage() {
                            if let Some(index) = self.memory.open_index(&storage) {
                                let provider = if let Some(ref params) = self.memory.backend_params
                                {
                                    params.make_embedding_provider().await
                                } else {
                                    None
                                };
                                let threshold = self
                                    .memory
                                    .flush_config
                                    .semantic_dedup_threshold
                                    .unwrap_or(SEMANTIC_DEDUP_SIMILARITY_THRESHOLD);
                                is_semantically_duplicate(
                                    &content,
                                    &index,
                                    provider.as_ref().map(|p| {
                                        p as &dyn crate::session::memory::embedding::EmbeddingProvider
                                    }),
                                    threshold,
                                )
                                .await
                            } else {
                                false
                            }
                        } else {
                            false
                        };

                        if is_sem_dup {
                            tracing::info!(
                                "memory flush: semantic duplicate detected, skipping write"
                            );
                            (
                                "semantic duplicate".to_string(),
                                resp_len,
                                acc_len,
                                truncated,
                                None,
                            )
                        } else if let Some(storage) = self.memory.storage() {
                            let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
                            let session_id = &self.session_info.id.0;
                            match storage
                                .write_daily_log(&date, trigger, session_id, &content, true)
                            {
                                Ok(path) => {
                                    tracing::info!("memory flush wrote session log");
                                    self.reindex_and_embed(&path, "session").await;
                                    *self.memory.last_flush_content.borrow_mut() = Some(content);
                                    (
                                        "written".to_string(),
                                        resp_len,
                                        acc_len,
                                        truncated,
                                        Some(path.display().to_string()),
                                    )
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "memory flush write failed");
                                    (
                                        format!("write failed: {e}"),
                                        resp_len,
                                        acc_len,
                                        truncated,
                                        None,
                                    )
                                }
                            }
                        } else {
                            (
                                "storage not configured".to_string(),
                                resp_len,
                                acc_len,
                                truncated,
                                None,
                            )
                        }
                    }
                    FlushResult::Rejected(reason) => {
                        tracing::warn!(reason = %reason, "memory flush response rejected");
                        (format!("rejected: {reason}"), resp_len, 0, false, None)
                    }
                }
            }
            Err(e) => {
                let detail = e
                    .data
                    .as_ref()
                    .and_then(|d| d.as_str())
                    .unwrap_or("memory flush failed");
                tracing::warn!(error = detail, "memory flush failed, skipping");
                (format!("skipped: {detail}"), 0, 0, false, None)
            }
        };

        tracing::info!(target: xai_grok_telemetry::memory_log::TARGET, outcome = %outcome, "MEMORY_FLUSH: completed");
        let flush_outcome = if outcome.starts_with("written") {
            "written"
        } else if outcome.starts_with("nothing") {
            "nothing_to_store"
        } else if outcome.starts_with("rejected") {
            "rejected"
        } else if outcome.starts_with("semantic duplicate") {
            "nothing_to_store"
        } else {
            "error"
        };
        self.memory.record_flush_result(flush_outcome);
        xai_grok_telemetry::session_ctx::log_event(
            xai_grok_telemetry::memory_telemetry::MemoryFlushComplete {
                session_id: self.session_info.id.to_string(),
                trigger: trigger.to_owned(),
                outcome: flush_outcome.to_owned(),
                duration_ms: flush_start.elapsed().as_millis() as u64,
                response_length: response_len,
                accepted_length: accepted_len,
                was_truncated,
            },
        );

        // Rolling session summary on each flush — crash-safe telemetry.
        let total_chunks = self
            .memory
            .storage
            .borrow()
            .as_ref()
            .map_or(0, |s| s.total_chunk_count());
        let telem = self.memory.telemetry_snapshot();
        self.emit_memory_session_summary(&telem, total_chunks, "flush_checkpoint");

        let flush_trigger = match trigger {
            "slash_command" => xai_grok_telemetry::events::MemoryFlushTrigger::SlashCommand,
            "interval" => xai_grok_telemetry::events::MemoryFlushTrigger::Interval,
            "pre_compaction" => xai_grok_telemetry::events::MemoryFlushTrigger::PreCompaction,
            _ => xai_grok_telemetry::events::MemoryFlushTrigger::UserRequested,
        };
        xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::MemoryFlushed {
            trigger: flush_trigger,
            success: flush_outcome == "written",
            duration_ms: flush_start.elapsed().as_millis() as u64,
            response_length: response_len,
        });

        self.memory.release_flush_lock();
        self.send_xai_notification(XaiSessionUpdate::MemoryFlushCompleted {
            result: outcome,
            path: flush_path,
        })
        .await;
        true
    }

    /// Capture the flush inputs before compaction mutates conversation history.
    pub(super) async fn snapshot_memory_flush_state(&self) -> MemoryFlushSnapshot {
        let (counts, conversation) = tokio::join!(
            self.chat_state_handle.get_conversation_counts(),
            self.chat_state_handle.get_conversation(),
        );
        let chat_history = crate::sampling::conversation_to_chat_messages(
            xai_chat_state::compaction_utils::prepare_conversation_for_summarization(conversation),
        );
        MemoryFlushSnapshot {
            counts,
            chat_history,
        }
    }

    /// Rewrite a raw memory note into well-structured markdown via a one-shot
    /// LLM call using the `grok-build` model.
    ///
    /// Follows the same streaming pattern as [`handle_ai_suggest`]: prepares
    /// a sampling client, builds a system+user prompt, streams the response,
    /// and returns the collected text.
    pub(super) async fn handle_rewrite_memory_note(
        &self,
        raw_text: &str,
        context_summary: &str,
    ) -> Result<String, String> {
        // Upper-bound check to prevent unbounded LLM input.
        const MAX_INPUT_BYTES: usize = 32 * 1024; // 32 KB
        let combined_len = raw_text.len() + context_summary.len();
        if combined_len > MAX_INPUT_BYTES {
            return Err(format!(
                "memory note input too large ({combined_len} bytes, max {MAX_INPUT_BYTES})"
            ));
        }

        let sampling_client = self
            .prepare_chat_completion(false)
            .await
            .map_err(|e| format!("failed to prepare client: {e}"))?;

        let system = "You are a memory note formatter. Rewrite the user's note into \
            well-structured markdown suitable for a persistent MEMORY.md file. The note should be:\n\
            - Concise but complete\n\
            - Start with a descriptive ## heading\n\
            - Include enough context to be useful months later\n\
            - Reference specific files, decisions, or patterns when relevant\n\
            - Use bullet points for multiple items\n\
            - Do NOT include timestamps or session IDs\n\
            - Do NOT add information that is not present in the original note\n\n\
            Return ONLY the formatted markdown, no explanations.";

        let user_msg = format!(
            "Session context:\n{context_summary}\n\nRewrite this note as a memory entry:\n\n{raw_text}"
        );

        let items = vec![
            ConversationItem::system(system.to_owned()),
            ConversationItem::user(user_msg),
        ];

        let request = ConversationRequest {
            items,
            tools: vec![],
            model: Some("grok-build".to_owned()),
            temperature: Some(0.3),
            max_output_tokens: Some(1024),
            ..Default::default()
        };

        let request_id = xai_grok_sampler::RequestId::random();
        let idle_timeout = std::time::Duration::from_secs(15);

        let result = match sampling_client.api_backend() {
            crate::sampling::ApiBackend::ChatCompletions => {
                let (raw, meta) = sampling_client
                    .conversation_stream(request)
                    .await
                    .map_err(|e| format!("rewrite stream failed: {e}"))?;
                let events =
                    xai_grok_sampler::stream_chat_completions(raw, meta, request_id, idle_timeout);
                xai_grok_sampler::collect_response(events).await
            }
            crate::sampling::ApiBackend::Responses => {
                let (raw, meta, doom_loop) = sampling_client
                    .conversation_stream_responses(request)
                    .await
                    .map_err(|e| format!("rewrite stream failed: {e}"))?;
                let events = xai_grok_sampler::stream_responses(
                    raw,
                    meta,
                    request_id,
                    idle_timeout,
                    doom_loop,
                );
                xai_grok_sampler::collect_response(events).await
            }
            crate::sampling::ApiBackend::Messages => {
                let (raw, meta) = sampling_client
                    .conversation_stream_messages(request)
                    .await
                    .map_err(|e| format!("rewrite stream failed: {e}"))?;
                let events = xai_grok_sampler::stream_messages(raw, meta, request_id, idle_timeout);
                xai_grok_sampler::collect_response(events).await
            }
        };

        match result {
            Ok((response, _metrics)) => {
                let text = response.assistant_text();
                if text.is_empty() {
                    Err("LLM returned empty response".to_string())
                } else {
                    Ok(text)
                }
            }
            Err(e) => {
                tracing::debug!(error = %e.message, "memory note rewrite inference failed");
                Err(format!("rewrite inference failed: {}", e.message))
            }
        }
    }
}
