//! Auxiliary model-call concern for `SessionActor`: side questions, recap
//! generation, and AI-suggest.

use super::*;

use crate::remote::DEFAULT_CONTEXT_WINDOW;
use crate::session::SideQuestionError;
use xai_grok_sampling_types::SamplingError;

/// Retry policy for the one-shot `/btw` model call: 3 attempts total
/// (1 try + 2 retries), 500ms → 1s jittered backoff. Deliberately short —
/// nothing like the sampler actor's budget — so a fleet-wide capacity event
/// can't multiply side-question traffic into a retry storm.
fn side_question_retry_policy() -> backon::ExponentialBuilder {
    backon::ExponentialBuilder::default()
        .with_max_times(2)
        .with_min_delay(std::time::Duration::from_millis(500))
        .with_max_delay(std::time::Duration::from_secs(1))
        .with_jitter()
}

/// Whether a failed `/btw` attempt is worth retrying: overload only (not
/// every retryable 5xx / stream glitch), minus the shared retry vetoes
/// (`x-should-retry: false`, context length — see
/// [`SamplingError::is_retry_vetoed`], also enforced by the sampler actor's
/// `classify_error`).
fn should_retry_side_question(e: &SamplingError) -> bool {
    e.is_overloaded() && !e.is_retry_vetoed()
}

/// Clone the base `/btw` request and stamp a fresh `req_id`, so retried
/// attempts never collide in logs. Everything else is byte-identical.
fn build_side_question_attempt(base: &ConversationRequest) -> ConversationRequest {
    let mut request = base.clone();
    request.x_grok_req_id = Some(format!("xai-btw-{}", uuid::Uuid::new_v4()));
    request
}

impl SessionActor {
    /// Handle a /btw side question — single-turn model call using the
    /// parent session's full context.
    ///
    /// Approach:
    /// - Keeps the parent's system prompt (conversation[0]) intact
    /// - Passes the full conversation history (including tool calls/results)
    /// - Includes tool definitions so the model knows capabilities
    /// - Wraps the question in a `<system-reminder>` block in a user message
    /// - Single turn, no tool execution
    ///
    /// Generates a unique btw session ID and persists the result to
    /// `btw_history.jsonl` in the session folder.
    pub(super) async fn handle_side_question(
        &self,
        question: &str,
    ) -> Result<String, SideQuestionError> {
        let btw_session_id = format!("btw-{}", uuid::Uuid::new_v4());
        let parent_session_id = self.session_info.id.to_string();
        let asked_at = chrono::Utc::now();

        let sampling_client = self
            .prepare_chat_completion(false)
            .await
            .map_err(|e| SideQuestionError::PrepareClient(e.to_string()))?;

        // Full conversation snapshot including system prompt, tool calls, and results.
        // Strip reasoning/thinking blocks from assistant items so we don't send
        // `ContentBlock::Thinking` without a top-level `thinking` config. The
        // Anthropic Messages API rejects requests that include thinking blocks in
        // messages but omit the `thinking` parameter.
        let mut items: Vec<ConversationItem> =
            xai_chat_state::compaction_utils::strip_reasoning_blocks(
                self.chat_state_handle.get_conversation().await,
            );

        // /btw fires mid-turn, so the snapshot may end with an assistant
        // message whose tool_calls have no matching ToolResult yet. The
        // Anthropic Messages API rejects this with "tool_use ids were found
        // without tool_result blocks". Truncate the trailing incomplete
        // assistant+tool_result run.
        while let Some(last) = items.last() {
            match last {
                ConversationItem::Assistant(a) if !a.tool_calls.is_empty() => {
                    items.pop();
                }
                ConversationItem::ToolResult(_) => {
                    items.pop();
                }
                _ => break,
            }
        }

        // Wrap the question in a <system-reminder> user message.
        let tag = self.reminder_wrapper_tag();
        let wrapped_question = format!(
            "<{tag}>This is a side question from the user. \
             You must answer this question directly in a single response.\n\n\
             IMPORTANT CONTEXT:\n\
             - You are a separate, lightweight agent spawned to answer this one question\n\
             - The main agent is NOT interrupted - it continues working independently in the background\n\
             - You share the conversation context but are a completely separate instance\n\
             - Do NOT reference being interrupted or what you were \"previously doing\" - that framing is incorrect\n\n\
             CRITICAL CONSTRAINTS:\n\
             - You have NO tools available - you cannot read files, run commands, search, or take any actions\n\
             - This is a one-off response - there will be no follow-up turns\n\
             - You can ONLY provide information based on what you already know from the conversation context\n\
             - NEVER say things like \"Let me try...\", \"I'll now...\", \"Let me check...\", or promise to take any action\n\
             - If you don't know the answer, say so - do not offer to look it up or investigate\n\n\
             Simply answer the question with the information you have.</{tag}>\n\n\
             {question}"
        );
        items.push(ConversationItem::user(wrapped_question));

        let tool_definitions = self.prepare_tool_definitions().await;
        let tool_specs: Vec<ToolSpec> = tool_definitions.into_iter().map(ToolSpec::from).collect();

        let model = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| c.model)
            .unwrap_or_default();

        let persist = |answer: String, success: bool, error: Option<String>, attempts: u32| {
            let _ = self.notifications.persistence_tx.send(PersistenceMsg::Btw(
                crate::session::persistence::BtwEntry {
                    btw_session_id: btw_session_id.clone(),
                    parent_session_id: parent_session_id.clone(),
                    asked_at,
                    question: question.to_string(),
                    answer,
                    model: model.clone(),
                    success,
                    error,
                    attempts,
                },
            ));
        };

        // Don't set temperature explicitly — cli-chat-proxy may inject
        // `thinking` config via request_defaults for thinking-enabled models,
        // Anthropic requires temperature == 1 when thinking is enabled.
        // Leaving it None lets the provider defaults apply correctly.
        //
        // Built once; each attempt clones it and stamps a fresh req_id (the
        // per-attempt clone is the cost of the owned-request API — retries
        // are rare, so the success path pays exactly one clone).
        let base_request = ConversationRequest {
            items,
            tools: tool_specs,
            model: Some(model.clone()),
            temperature: None,
            x_grok_conv_id: Some(btw_session_id.clone()),
            x_grok_session_id: Some(parent_session_id.clone()),
            x_grok_agent_id: Some(xai_grok_telemetry::id::agent_id()),
            ..Default::default()
        };

        // conversation_collect is one-shot (no sampler-actor retry); /btw adds
        // its own bounded overload-only retry (policy + predicate above).
        use backon::Retryable as _;
        let attempts = std::cell::Cell::new(1u32);
        let result =
            (|| sampling_client.conversation_collect(build_side_question_attempt(&base_request)))
                .retry(side_question_retry_policy())
                .when(should_retry_side_question)
                .notify(|e: &SamplingError, backoff: std::time::Duration| {
                    attempts.set(attempts.get() + 1);
                    tracing::warn!(
                        backoff_ms = backoff.as_millis() as u64,
                        error = %e,
                        "side question overload; retrying"
                    );
                })
                .await;

        match result {
            Ok(response) => {
                let content = response.assistant_text();
                if content.is_empty() {
                    let err = SideQuestionError::EmptyResponse;
                    persist(String::new(), false, Some(err.to_string()), attempts.get());
                    return Err(err);
                }
                persist(content.clone(), true, None, attempts.get());
                Ok(content)
            }
            Err(e) => {
                let err = SideQuestionError::from(e);
                persist(String::new(), false, Some(err.to_string()), attempts.get());
                Err(err)
            }
        }
    }

    /// Generate a session recap and broadcast it via
    /// [`SessionUpdate::SessionRecap`](crate::extensions::notification::SessionUpdate::SessionRecap).
    ///
    /// Snapshots the conversation, appends a single recap instruction turn
    /// (reusing the prompt prefix verbatim so the provider cache stays warm),
    /// makes one tool-free model call, and emits the cleaned one-line summary
    /// for display only. It never mutates the conversation.
    ///
    /// Best-effort: a failed or empty generation is logged and dropped — a
    ///
    /// missing recap must never disrupt the session.
    pub(super) async fn handle_recap(&self, auto: bool) {
        use crate::session::helpers::session_recap;

        // Snapshot before the first await so a prompt accepted while we await
        // the conversation reads as bumped-after-capture and cancels this recap.
        let recap_epoch = self.recap_epoch.get();

        let conversation = self.chat_state_handle.get_conversation().await;
        let main_turns = session_recap::main_turn_count(&conversation);

        let stored = self.last_recap_main_turn.get();
        let last = if stored > main_turns {
            let healed = main_turns.saturating_sub(1);
            self.last_recap_main_turn.set(healed);
            healed
        } else {
            stored
        };

        const RECAP_MIN_IDLE_MS: i64 = 3 * 60 * 1000;
        let last_ms = self
            .last_api_request_at
            .load(std::sync::atomic::Ordering::Relaxed);
        let idle_ms = chrono::Utc::now().timestamp_millis() - last_ms;
        let idle_ok = last_ms != 0 && idle_ms >= RECAP_MIN_IDLE_MS;

        if let Err(reason) = session_recap::recap_gate(main_turns, last, auto, idle_ok) {
            tracing::debug!(auto, main_turns, last, reason, "skipping recap");
            // A manual `/recap` shows a loading spinner; tell the client there
            // is nothing to recap so it can clear it (auto shows none).
            if !auto {
                self.emit_recap_unavailable().await;
            }
            return;
        }

        // Serialize recap work: watermark alone cannot exclude concurrent manual
        // re-recaps once last == main_turns (in-flight or finished).
        if self.recap_in_flight.get() {
            tracing::debug!(auto, main_turns, "skipping recap: another recap in flight");
            if !auto {
                self.emit_recap_unavailable().await;
            }
            return;
        }
        self.recap_in_flight.set(true);
        // Clear in-flight on every exit. Advance watermark only on success/suppress
        // (not on failure/empty/cancel) so auto can retry later for this turn if needed.
        let clear_in_flight = || self.recap_in_flight.set(false);

        let sampling_client = match self.prepare_chat_completion(false).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "recap: failed to prepare sampling client");
                clear_in_flight();
                // A manual `/recap` shows a loading spinner; clear it on failure.
                if !auto {
                    self.emit_recap_unavailable().await;
                }
                return;
            }
        };

        let tag = self.reminder_wrapper_tag();
        // Strip reasoning only on the Messages backend (it rejects thinking
        // blocks without a `thinking` config). Other backends keep reasoning
        // verbatim so the prefix matches the last turn and the prefix KV
        // cache stays warm. Mirrors compaction's `summary_strips_reasoning`.
        let strip_reasoning =
            sampling_client.api_backend() == crate::sampling::ApiBackend::Messages;

        // Budget off the recap model's context window (today the session model).
        // One read serves both the window and the model.
        let sampling_config = self.chat_state_handle.get_sampling_config().await;
        let context_window = sampling_config
            .as_ref()
            .map(|c| c.context_window.get())
            .unwrap_or(DEFAULT_CONTEXT_WINDOW);
        let items =
            session_recap::budget_recap_items(conversation, tag, strip_reasoning, context_window);

        let model = sampling_config.map(|c| c.model).unwrap_or_default();

        // Leave BOTH temperature and max_output_tokens unset: the cli-chat-proxy
        // layer may inject a `thinking` budget for thinking-enabled models
        // (which also forces temperature == 1), and a small max_output_tokens
        // below that budget makes the call error or return empty — silently
        // dropping the recap. The recap instruction keeps the body to
        // ~25–40 words, and `clean_recap_text` caps it at a generous
        // RECAP_MAX_CHARS safety net, so an explicit token cap isn't needed.
        let started_at = chrono::Utc::now().to_rfc3339();
        let x_grok_conv_id = format!("recap-{}", uuid::Uuid::new_v4());
        let x_grok_req_id = format!("xai-recap-{}", uuid::Uuid::new_v4());
        // Clone the exact request items for the on-disk artifact (recap never
        // mutates conversation state, so this file is the only durable record).
        let chat_history_for_artifact = items.clone();
        // Main-turn tool specs: tools serialize into the cached token prefix.
        let tool_defs = self.prepare_tool_definitions().await;
        let tools = self.turn_base_tool_specs(&tool_defs).await;
        // Mirror the main turn's hosted tools so a recap can't search past the active cutoff.
        let hosted_tools = self.hosted_tools_for_turn().await;
        let request = ConversationRequest {
            items,
            tools,
            hosted_tools,
            model: Some(model.clone()),
            temperature: None,
            x_grok_conv_id: Some(x_grok_conv_id.clone()),
            x_grok_req_id: Some(x_grok_req_id.clone()),
            x_grok_session_id: Some(self.session_info.id.to_string()),
            x_grok_agent_id: Some(xai_grok_telemetry::id::agent_id()),
            prompt_cache_key: Some(self.session_info.id.to_string()),
            ..Default::default()
        };

        let response = match sampling_client.conversation_collect(request).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "recap: model call failed");
                self.persist_recap_request_artifact(
                    chat_history_for_artifact,
                    &model,
                    auto,
                    strip_reasoning,
                    tag,
                    &x_grok_req_id,
                    &x_grok_conv_id,
                    started_at,
                    None,
                    None,
                    Some(&e.to_string()),
                );
                clear_in_flight();
                // A manual `/recap` shows a loading spinner; clear it on failure.
                if !auto {
                    self.emit_recap_unavailable().await;
                }
                return;
            }
        };

        let raw_response = response.assistant_text();
        let summary = session_recap::clean_recap_text(&raw_response);
        if summary.is_empty() {
            tracing::debug!("recap: model returned empty summary");
            self.persist_recap_request_artifact(
                chat_history_for_artifact,
                &model,
                auto,
                strip_reasoning,
                tag,
                &x_grok_req_id,
                &x_grok_conv_id,
                started_at,
                None,
                Some(raw_response.as_str()).filter(|s| !s.is_empty()),
                Some("empty summary after clean_recap_text"),
            );
            clear_in_flight();
            // A manual `/recap` shows a loading spinner; clear it when empty.
            if !auto {
                self.emit_recap_unavailable().await;
            }
            return;
        }

        // New prompt while generating: keep artifact, skip display, leave watermark.
        // Applies to manual `/recap` too: spinner-less clients (e.g. Grok
        // Desktop) would otherwise append the late recap mid-turn.
        if self.recap_was_cancelled(recap_epoch) {
            tracing::info!(
                auto,
                recap_epoch,
                current_epoch = self.recap_epoch.get(),
                "session recap cancelled (new prompt while generating; not shown)"
            );
            self.persist_recap_request_artifact(
                chat_history_for_artifact,
                &model,
                auto,
                strip_reasoning,
                tag,
                &x_grok_req_id,
                &x_grok_conv_id,
                started_at,
                Some(summary.as_str()),
                Some(raw_response.as_str()),
                Some("cancelled: new prompt while recap generating"),
            );
            self.drop_recap_after_cancel(auto).await;
            return;
        }

        // Auto long-tail: save artifact, do not show. Manual always shows.
        if auto && session_recap::should_suppress_auto_recap_display(&raw_response, &summary) {
            tracing::info!(
                raw_bytes = raw_response.len(),
                summary_bytes = summary.len(),
                "session recap suppressed (auto long-tail; artifact saved, not shown)"
            );
            self.persist_recap_request_artifact(
                chat_history_for_artifact,
                &model,
                auto,
                strip_reasoning,
                tag,
                &x_grok_req_id,
                &x_grok_conv_id,
                started_at,
                Some(summary.as_str()),
                Some(raw_response.as_str()),
                Some("auto recap suppressed: long-tail output not shown"),
            );
            // Commit watermark only if still live (no await between check and mark).
            let _ = self.try_commit_recap(recap_epoch, main_turns);
            return;
        }

        tracing::info!(auto, chars = summary.len(), "session recap generated");
        self.persist_recap_request_artifact(
            chat_history_for_artifact,
            &model,
            auto,
            strip_reasoning,
            tag,
            &x_grok_req_id,
            &x_grok_conv_id,
            started_at,
            Some(summary.as_str()),
            Some(raw_response.as_str()),
            None,
        );
        // Final cancel check immediately before mark+emit (no await between).
        if !self.try_commit_recap(recap_epoch, main_turns) {
            if !auto {
                self.emit_recap_unavailable().await;
            }
            return;
        }
        self.send_xai_notification(
            crate::extensions::notification::SessionUpdate::SessionRecap { summary, auto },
        )
        .await;
    }

    /// Invalidate in-flight recap (real user prompt at queue time / turn start).
    pub(crate) fn cancel_pending_recap_for_new_prompt(&self) {
        self.recap_epoch.set(self.recap_epoch.get().wrapping_add(1));
    }

    /// Whether `epoch` is stale because a newer prompt started.
    pub(crate) fn recap_was_cancelled(&self, epoch: u64) -> bool {
        self.recap_epoch.get() != epoch
    }

    /// If still live, advance watermark and clear in-flight; else clear only.
    /// Returns whether the recap may emit (or count as done for suppress).
    pub(crate) fn try_commit_recap(&self, recap_epoch: u64, main_turns: usize) -> bool {
        if self.recap_was_cancelled(recap_epoch) {
            self.recap_in_flight.set(false);
            false
        } else {
            self.last_recap_main_turn.set(main_turns);
            self.recap_in_flight.set(false);
            true
        }
    }

    /// Cancel-branch cleanup after generation: clear in-flight; manual clients
    /// get `SessionRecapUnavailable` so their spinner can clear.
    pub(crate) async fn drop_recap_after_cancel(&self, auto: bool) {
        self.recap_in_flight.set(false);
        if !auto {
            self.emit_recap_unavailable().await;
        }
    }

    /// Persist a recap request artifact for offline prompt / garble analysis.
    /// Writes `{session_dir}/recap_requests/{request_id}.json` containing the
    /// exact `ConversationItem` list sent to the model plus the cleaned
    /// summary and raw assistant text (or error). Rides on the post-turn
    /// session archive to cloud storage like compaction request artifacts.
    /// Best-effort: send-failures are logged at `warn` and never surfaced —
    /// clear the loading spinner it is showing instead of animating forever.
    /// a missing artifact must never disrupt recap display.
    #[allow(clippy::too_many_arguments)]
    fn persist_recap_request_artifact(
        &self,
        chat_history: Vec<ConversationItem>,
        model: &str,
        auto: bool,
        strip_reasoning: bool,
        reminder_tag: &str,
        x_grok_req_id: &str,
        x_grok_conv_id: &str,
        started_at: String,
        summary: Option<&str>,
        raw_response: Option<&str>,
        error: Option<&str>,
    ) {
        use crate::extensions::notification::RecapRequestFile;
        use crate::session::persistence::PersistenceMsg;

        let artifact = RecapRequestFile {
            schema_version: 1,
            request_id: uuid::Uuid::new_v4().to_string(),
            created_at: started_at,
            trigger: if auto { "auto" } else { "manual" }.to_owned(),
            model: model.to_owned(),
            x_grok_req_id: x_grok_req_id.to_owned(),
            x_grok_conv_id: x_grok_conv_id.to_owned(),
            strip_reasoning,
            reminder_tag: reminder_tag.to_owned(),
            chat_history,
            summary: summary.map(str::to_owned),
            raw_response: raw_response.map(str::to_owned),
            error: error.map(str::to_owned),
        };

        if self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::RecapRequest(artifact))
            .is_err()
        {
            tracing::warn!(
                session_id = %self.session_info.id.0,
                "Failed to send recap request artifact to persistence channel"
            );
        }
    }

    /// Tell the live client that a manual `/recap` produced no recap, so it can
    ///
    /// Only the manual path shows a spinner, so callers gate this on `!auto`.
    async fn emit_recap_unavailable(&self) {
        self.send_xai_notification(
            crate::extensions::notification::SessionUpdate::SessionRecapUnavailable,
        )
        .await;
    }

    /// Handle an AI-powered shell command suggestion request.
    /// Builds a minimal prompt from the prefix and CWD, calls the sampler
    ///
    /// with low temperature and small max_tokens, and returns the suggestion.
    pub(super) async fn handle_ai_suggest(
        &self,
        prefix: &str,
        cwd: &str,
        model_override: Option<&str>,
    ) -> Option<String> {
        let sampling_client = self.prepare_chat_completion(false).await.ok()?;

        let system = "You are a shell command autocomplete engine. \
            Given a partial command, output ONLY the completed command. \
            No explanation, no markdown, no quotes. Just the raw command.";

        let user_msg = format!("CWD: {cwd}\nPartial command: {prefix}");

        let items = vec![
            ConversationItem::system(system.to_owned()),
            ConversationItem::user(user_msg),
        ];

        let model = match model_override {
            Some(m) => m.to_owned(),
            None => "grok-build".to_owned(),
        };

        let request = ConversationRequest {
            items,
            tools: vec![],
            model: Some(model),
            temperature: Some(0.1),
            max_output_tokens: Some(50),
            ..Default::default()
        };

        let request_id = xai_grok_sampler::RequestId::random();
        let idle_timeout = std::time::Duration::from_secs(5);

        let result = match sampling_client.api_backend() {
            crate::sampling::ApiBackend::ChatCompletions => {
                let (raw, meta) = sampling_client.conversation_stream(request).await.ok()?;
                let events =
                    xai_grok_sampler::stream_chat_completions(raw, meta, request_id, idle_timeout);
                xai_grok_sampler::collect_response(events).await
            }
            crate::sampling::ApiBackend::Responses
            | crate::sampling::ApiBackend::CodexResponses => {
                let (raw, meta, doom_loop) = sampling_client
                    .conversation_stream_responses(request)
                    .await
                    .ok()?;
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
                    .ok()?;
                let events = xai_grok_sampler::stream_messages(raw, meta, request_id, idle_timeout);
                xai_grok_sampler::collect_response(events).await
            }
            crate::sampling::ApiBackend::GoogleGenerateContent => {
                let (raw, identity) = sampling_client
                    .conversation_stream_google(request)
                    .await
                    .ok()?;
                let events = xai_grok_sampler::google::stream_google_generate_content(
                    raw,
                    request_id,
                    identity,
                    idle_timeout,
                );
                xai_grok_sampler::collect_response(events).await
            }
            crate::sampling::ApiBackend::BedrockConverseStream => {
                let (raw, identity) = sampling_client
                    .conversation_stream_bedrock(request)
                    .await
                    .ok()?;
                let events = xai_grok_sampler::bedrock::stream_bedrock_converse(
                    raw,
                    request_id,
                    identity,
                    idle_timeout,
                );
                xai_grok_sampler::collect_response(events).await
            }
            crate::sampling::ApiBackend::PiMessages => {
                let (raw, identity) = sampling_client
                    .conversation_stream_pi_messages(request)
                    .await
                    .ok()?;
                let events = xai_grok_sampler::pi_messages::stream_pi_messages(
                    raw,
                    request_id,
                    identity,
                    idle_timeout,
                );
                xai_grok_sampler::collect_response(events).await
            }
        };

        match result {
            Ok((response, _metrics)) => {
                let text = response.assistant_text();
                if text.is_empty() { None } else { Some(text) }
            }
            Err(e) => {
                tracing::debug!(error = %e.message, "AI suggest inference failed");
                None
            }
        }
    }

    /// Predict the user's likely next prompt for tab-autocomplete ghost text.
    /// Fired by the client after a turn completes. Builds a compact text-only
    /// transcript of the recent conversation (see
    /// [`prompt_suggest::build_transcript`]) and makes one tool-free model
    /// call. The model is resolved by
    /// [`prompt_suggest::effective_suggest_model`]: env
    /// (`GROK_PROMPT_SUGGESTIONS_MODEL`) > `[models] prompt_suggestion`
    /// (config.toml) > remote `prompt_suggestion_model` (remote settings) >
    /// (config.toml) > remote `prompt_suggestion_model` (remote settings) >
    /// [`prompt_suggest::DEFAULT_SUGGEST_MODEL`] (`grok-build-0.1`). Every
    /// tier except env is catalog-guarded against this shell's own model
    /// catalog — when the effective model is not sampleable here (e.g.
    /// `grok-build-0.1` for OAuth users) the request is **skipped
    /// entirely** instead of fired doomed. The session model is never used:
    /// a per-turn background call must stay on the small model.
    /// Temperature, max_output_tokens, and
    /// reasoning_effort are left unset — mirrors [`Self::handle_recap`]: the
    /// proxy may inject provider defaults, a small token cap silently empties
    /// a reasoning model's response, and some models (e.g. `grok-build`)
    /// reject an explicit `reasoningEffort` with a 400. Output is filtered
    /// through [`prompt_suggest::sanitize_suggestion`]; any failure returns
    /// through [`prompt_suggest::sanitize_suggestion`]; any failure returns
    /// `None`.
    pub(super) async fn handle_suggest_prompt(
        &self,
        model_override: Option<&str>,
    ) -> Option<String> {
        use crate::session::helpers::prompt_suggest;

        let pin = self.models_manager.prompt_suggest_model_pin();
        let Some(model) = prompt_suggest::effective_suggest_model(&pin, model_override, |m| {
            self.models_manager.model_in_catalog(m)
        }) else {
            tracing::debug!(
                pin = ?pin,
                client_hint = ?model_override,
                "prompt suggest: effective model not in catalog; skipping request"
            );
            return None;
        };

        let conversation = self.chat_state_handle.get_conversation().await;
        let Some(transcript) = prompt_suggest::build_transcript(&conversation) else {
            tracing::debug!(
                items = conversation.len(),
                "prompt suggest: no usable transcript"
            );
            return None;
        };

        let sampling_client = match self.prepare_chat_completion(false).await {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(error = %e, "prompt suggest: sampling client unavailable");
                return None;
            }
        };

        tracing::debug!(
            model = %model,
            transcript_len = transcript.len(),
            "prompt suggest: requesting"
        );

        let cwd = self
            .tool_context
            .cwd
            .as_path()
            .to_string_lossy()
            .into_owned();
        let items = vec![
            ConversationItem::system(prompt_suggest::SUGGEST_PROMPT_SYSTEM.to_owned()),
            ConversationItem::user(prompt_suggest::suggest_prompt_user_message(
                &transcript,
                &cwd,
            )),
        ];

        let request = ConversationRequest {
            items,
            tools: vec![],
            model: Some(model),
            temperature: None,
            x_grok_conv_id: Some(format!("promptsuggest-{}", uuid::Uuid::new_v4())),
            x_grok_req_id: Some(format!("xai-promptsuggest-{}", uuid::Uuid::new_v4())),
            x_grok_session_id: Some(self.session_info.id.to_string()),
            x_grok_agent_id: Some(xai_grok_telemetry::id::agent_id()),
            ..Default::default()
        };

        let response = match sampling_client.conversation_collect(request).await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(error = %e, "prompt suggest inference failed");
                return None;
            }
        };

        let raw = response.assistant_text();
        let mut suggestion = prompt_suggest::sanitize_suggestion(&raw);
        // Deterministic anti-repeat backstop: never ghost a multi-word
        // prompt the user already sent (the prompt asks the model not to,
        // but a filter guarantees it).
        if let Some(s) = &suggestion
            && prompt_suggest::is_repeat_of_user_message(s, &conversation)
        {
            tracing::debug!("prompt suggest: rejected repeat of a past user prompt");
            suggestion = None;
        }
        tracing::debug!(
            raw_preview = %xai_grok_tools::util::truncate_str(raw.trim(), 60),
            accepted = suggestion.is_some(),
            "prompt suggest: response"
        );
        suggestion
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(status: u16, message: &str, should_retry: Option<bool>) -> SamplingError {
        SamplingError::Api {
            status: reqwest::StatusCode::from_u16(status).unwrap(),
            message: message.into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry,
        }
    }

    #[test]
    fn side_question_retries_overload_only() {
        // Stream overload and its proxy-wrapped 500 shape retry; so does 529.
        assert!(should_retry_side_question(&SamplingError::StreamError {
            error_type: "overloaded_error".into(),
            message: "Overloaded".into(),
        }));
        assert!(should_retry_side_question(&api(
            500,
            "stream error (overloaded_error): Overloaded",
            None
        )));
        assert!(should_retry_side_question(&api(529, "capacity", None)));

        // Server veto (`x-should-retry: false`) wins over overload.
        assert!(!should_retry_side_question(&api(
            529,
            "capacity",
            Some(false)
        )));
        // Deterministic context-length failures never retry, even on 529.
        assert!(!should_retry_side_question(&api(
            529,
            "invalid_request_error: prompt is too long: 300000 tokens > 200000 maximum",
            None
        )));
        // Rate limit and generic 5xx are not overload — no /btw retry.
        assert!(!should_retry_side_question(&api(429, "slow down", None)));
        assert!(!should_retry_side_question(&api(
            503,
            "upstream connect timeout",
            None
        )));
    }

    /// The wired policy: 3 attempts total, backoff within the configured
    /// bounds (500ms + 1s base, jitter adds up to the current delay), and a
    /// fresh request id stamped per attempt.
    #[tokio::test(start_paused = true)]
    async fn side_question_retry_wiring_caps_attempts_and_bounds_backoff() {
        use backon::Retryable as _;

        let calls = std::cell::Cell::new(0u32);
        let start = tokio::time::Instant::now();
        let result: Result<(), SamplingError> = (|| async {
            calls.set(calls.get() + 1);
            Err(SamplingError::StreamError {
                error_type: "overloaded_error".into(),
                message: "Overloaded".into(),
            })
        })
        .retry(side_question_retry_policy())
        .when(should_retry_side_question)
        .await;

        assert!(result.is_err());
        assert_eq!(calls.get(), 3, "1 try + 2 retries");
        // Base delays 500ms + 1s; jitter adds (0, delay) per sleep.
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(1_500),
            "elapsed {elapsed:?} below minimum backoff"
        );
        assert!(
            elapsed <= std::time::Duration::from_millis(3_100),
            "elapsed {elapsed:?} above maximum backoff"
        );
    }

    #[test]
    fn side_question_attempts_get_fresh_request_ids() {
        let base = ConversationRequest {
            x_grok_conv_id: Some("btw-test".into()),
            ..Default::default()
        };
        let a = build_side_question_attempt(&base);
        let b = build_side_question_attempt(&base);
        let (a_id, b_id) = (a.x_grok_req_id.unwrap(), b.x_grok_req_id.unwrap());
        assert!(a_id.starts_with("xai-btw-"));
        assert_ne!(a_id, b_id, "each attempt must get a fresh req_id");
        // Everything except the request id is byte-identical to the base.
        assert_eq!(a.x_grok_conv_id, base.x_grok_conv_id);
    }
}
