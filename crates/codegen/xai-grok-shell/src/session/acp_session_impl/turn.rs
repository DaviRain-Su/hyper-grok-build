//! Turn-execution concern for `SessionActor` (`handle_prompt`, turn-end,
//! sampling loop).
use super::*;
/// Synthetic tool the model calls to return its schema-constrained final answer
/// on backends that can't constrain output natively (Messages API). Intercepted
/// in the loop, never executed as a real tool.
const STRUCTURED_OUTPUT_TOOL: &str = "StructuredOutput";
/// Max times the model may re-call `StructuredOutput` with non-conforming args
/// before the turn ends with the last validation error.
const STRUCTURED_OUTPUT_MAX_RETRIES: u32 = 3;
/// What a `StructuredOutput` tool call means for the turn (see
/// `handle_structured_output_tool_call`).
enum StructuredOutputStep {
    /// Accepted, or retries exhausted: the carried result is the final output.
    Complete(Result<serde_json::Value, String>),
    /// Non-conforming args; a corrective tool_result was pushed — re-sample.
    Retry,
    /// No sole StructuredOutput call (absent, or co-emitted with real tools that
    /// should run this round).
    Proceed,
}
/// Parse `raw` as JSON and validate it against a `validator` compiled once per
/// turn. Returns the value on success, or a human-readable error (surfaced to
/// the model on retry and to the client as `structuredOutputError`). A `validator`
/// of `Err` means the user's schema itself was invalid.
fn validate_structured_output(
    validator: &Result<jsonschema::Validator, String>,
    raw: &str,
) -> Result<serde_json::Value, String> {
    let validator = validator.as_ref().map_err(Clone::clone)?;
    let value: serde_json::Value = serde_json::from_str(raw.trim())
        .map_err(|e| format!("model output was not valid JSON: {e}"))?;
    match validator.validate(&value) {
        Ok(()) => Ok(value),
        Err(e) => Err(format!("output does not match the required schema: {e}")),
    }
}
/// Result of the turn-end usage drain (and cancel's no-drain snapshot).
///
/// **Ledger marks** only when [`Self::fail_closed`]. Sticky and background
/// live are **report-level only** (tokens still land on the session ledger).
pub(super) struct UsageDrainOutcome {
    /// Query failure, FG still live after timeout/cancel. Marks both
    /// the prompt and session bills incomplete. (True apply-miss stains
    /// ledgers at fold time via `mark_apply_miss_incomplete`, not here.)
    pub(super) fail_closed: bool,
    /// A background child is still running: only this prompt's report is
    /// incomplete; its spend reaches the session ledger at completion.
    pub(super) background_live: bool,
    /// Pin-scoped sticky (session-only attribution or apply-miss report).
    /// Report incomplete only — does not stain ledgers by itself.
    pub(super) sticky_report: bool,
}
impl UsageDrainOutcome {
    /// Wire / attach incomplete: fail-closed ∪ background ∪ sticky.
    pub(super) fn report_incomplete(&self) -> bool {
        self.fail_closed || self.background_live || self.sticky_report
    }
    /// Map an outstanding reply without a multi-second drain (cancel path).
    /// Same policy as freeze's terminal outcome: FG live → fail-closed;
    /// sticky and background → report only.
    pub(super) fn from_outstanding_reply(
        reply: Option<
            &xai_grok_tools::implementations::grok_build::task::types::SubagentOutstandingReply,
        >,
    ) -> Self {
        match reply {
            None => Self {
                fail_closed: true,
                background_live: false,
                sticky_report: false,
            },
            Some(r) => Self {
                fail_closed: !r.live_ids.is_empty(),
                background_live: r.background_live,
                sticky_report: r.subagent_usage_not_applied,
            },
        }
    }
}
/// Accumulates a turn's per-call token usage and tool-call presence across the
/// agentic loop's model calls, recording running totals on the turn span. Kept
/// out of the loop body so telemetry bookkeeping doesn't obscure control flow.
#[derive(Default)]
struct TurnSpanTotals {
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    has_tool_call: bool,
}
impl TurnSpanTotals {
    /// Fold one model response into the totals (tokens sum — each call is billed
    /// its full prompt; has_tool_call OR-s — the final call has none) and update
    /// the span. `stop_reason` is last-wins (the terminal reason), not summed.
    fn record(&mut self, span: &tracing::Span, response: &ConversationResponse) {
        if let Some(u) = response.usage.as_ref() {
            self.input_tokens += i64::from(u.prompt_tokens);
            self.output_tokens += i64::from(u.completion_tokens);
            self.cache_read_tokens += i64::from(u.cached_prompt_tokens);
            span.record("input_tokens", self.input_tokens);
            span.record("output_tokens", self.output_tokens);
            span.record("cache_read_tokens", self.cache_read_tokens);
        }
        if let Some(sr) = response.stop_reason {
            span.record("stop_reason", sr.as_str());
        }
        self.has_tool_call |= !response.tool_calls().is_empty();
        span.record("response.has_tool_call", self.has_tool_call);
    }
}
/// How the turn's per-block user-message echo is published to clients /
/// `updates.jsonl`.
///
/// Every turn consumes a `prompt_index`, and rewind / fork truncation
/// (`replay_to_prompt`, `updates_truncate_for_prompt`) recover turn
/// boundaries by counting persisted `UserMessageChunk` runs — so every mode
/// persists the echo. Turns whose content must not render as a user prompt
/// (notification drain) are hidden by the *pager* via the
/// `hideFromScrollback` chunk meta, not by omitting the persisted line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserEchoMode {
    /// Live + persist (real user / cron / skill turns).
    Broadcast,
    /// Persist without live broadcast. Interject-fallback: panes already
    /// rendered the text, so a live echo would duplicate it. Notification
    /// drain: model-only content (the UI surfaces it via side channels:
    /// monitor gutter, task pane) that no pane should render live.
    PersistOnly,
}
fn user_echo_mode(prompt_id: &str) -> UserEchoMode {
    if prompt_id.starts_with(super::interjection::INTERJECT_FALLBACK_PROMPT_PREFIX) {
        return UserEchoMode::PersistOnly;
    }
    match super::super::PromptOrigin::from_prompt_id(prompt_id) {
        super::super::PromptOrigin::NotificationDrain => UserEchoMode::PersistOnly,
        _ => UserEchoMode::Broadcast,
    }
}
impl SessionActor {
    /// Run the image-normalization pipeline (re-encode caps, min-side and
    /// integrity checks) and surface its outcomes: compression / re-encode
    /// fallback / dropped notices are appended to `text_out` (TEXT only —
    /// image data never enters a string) and mirrored as
    /// `ImageCompressed`/`ImageDropped` notifications. Returns the surviving
    /// images. Single owner of the notice/notify wiring, shared by the
    /// prompt path and the interjection drain.
    pub(crate) async fn normalize_images_with_notices(
        &self,
        text_out: &mut String,
        images: Vec<acp::ImageContent>,
        is_cursor: bool,
    ) -> Vec<acp::ImageContent> {
        let mut norm_result =
            crate::session::image_normalize::normalize_images(images, is_cursor).await;
        let user_images = std::mem::take(&mut norm_result.images);
        use crate::extensions::notification::ImageCompressedEntry;
        if !norm_result.compressed.is_empty() {
            text_out.push_str(&crate::session::image_normalize::render_compression_notice(
                &norm_result.compressed,
                is_cursor,
            ));
            let message = norm_result
                .compressed
                .iter()
                .map(|c| c.display())
                .collect::<Vec<_>>()
                .join("; ");
            let images = norm_result
                .compressed
                .iter()
                .map(ImageCompressedEntry::from)
                .collect();
            self.send_xai_notification(XaiSessionUpdate::ImageCompressed { images, message })
                .await;
        }
        if !norm_result.re_encode_fallbacks.is_empty() {
            text_out.push_str(
                &crate::session::image_normalize::render_re_encode_fallback_notice(
                    &norm_result.re_encode_fallbacks,
                    is_cursor,
                ),
            );
            self.send_xai_notification(XaiSessionUpdate::ImageCompressed {
                images: vec![],
                message: norm_result.re_encode_fallbacks.join(" "),
            })
            .await;
        }
        if let Some((notice, notes)) = crate::session::image_normalize::dropped_to_envelope(
            std::mem::take(&mut norm_result.dropped),
            is_cursor,
        ) {
            text_out.push_str(&notice);
            self.send_xai_notification(XaiSessionUpdate::ImageDropped { notes })
                .await;
        }
        user_images
    }
    pub(super) fn persist_host_turn_user_echo(&self, text: &str, prompt_id: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let mut chunk_meta = serde_json::Map::new();
        chunk_meta.insert(
            crate::session::storage::HOST_TURN_META_KEY.into(),
            serde_json::json!(true),
        );
        if super::super::PromptOrigin::from_prompt_id(prompt_id).hide_user_echo_from_scrollback() {
            chunk_meta.insert("hideFromScrollback".into(), serde_json::json!(true));
        }
        let update = acp::SessionUpdate::UserMessageChunk(
            acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
                text.to_string(),
            )))
            .meta(Some(chunk_meta)),
        );
        let notification_meta = self.build_notification_meta();
        let notification = acp::SessionNotification::new(self.session_info.id.clone(), update)
            .meta(notification_meta.as_object().cloned());
        let _ = self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::Update(
                crate::session::storage::SessionUpdate::Acp(Box::new(notification)),
            ));
    }
    #[tracing::instrument(
        name = "session.handle_prompt",
        skip_all,
        fields(
            session_id = %self.session_info.id.0,
            prompt_id = %prompt_id,
            prompt_length = tracing::field::Empty,
            command_name = tracing::field::Empty,
            command_source = tracing::field::Empty,
        )
    )]
    pub(super) async fn handle_prompt(
        self: &Arc<Self>,
        prompt_id: &str,
        prompt_blocks: Vec<acp::ContentBlock>,
        prompt_mode: PromptMode,
        trace_gcs_config: Option<crate::session::repo_changes::TraceExportConfig>,
        artifact_tracker: Option<crate::upload::manifest::ArtifactTracker>,
        prompt_client_identifier: Option<String>,
        prompt_screen_mode: Option<String>,
        verbatim: bool,
        json_schema: Option<serde_json::Value>,
        persist_ack: Option<oneshot::Sender<()>>,
        parsed_prompt_tx: Option<oneshot::Sender<ParsedPromptInfo>>,
    ) -> PromptTurnResult {
        let handle_prompt_start = std::time::Instant::now();
        let prompt_length: usize = prompt_blocks
            .iter()
            .map(|b| match b {
                acp::ContentBlock::Text(t) => t.text.len(),
                _ => 0,
            })
            .sum();
        tracing::Span::current().record("prompt_length", prompt_length as i64);
        *self.active_skill.lock() = None;
        xai_grok_telemetry::unified_log::info(
            "shell.handle_prompt.start",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "prompt_id": prompt_id,
                "block_count": prompt_blocks.len(),
            })),
        );
        let origin = super::super::PromptOrigin::from_prompt_id(prompt_id);
        if let Some(completion_id) = origin.completion_id() {
            self.mark_completions_reported(&[completion_id]).await;
            if let Some(reservations) = &self.tool_context.task_completion_reservations {
                reservations.release(completion_id);
            }
        }
        if !origin.is_synthetic() {
            self.cancel_pending_recap_for_new_prompt();
        }
        *self.turn_start_prompt_mode.lock() = prompt_mode;
        *self.turn_prompt_mode.lock() = prompt_mode;
        self.signals_handle().increment_turn();
        self.reconcile_plan_mode_with_prompt(prompt_mode);
        let _turn_active_guard =
            TurnActiveGuard::activate(self.tool_context.is_turn_active.as_ref());
        let _session_turn_active_guard = TurnActiveGuard::activate(Some(&self.session_turn_active));
        let turn_start_input = xai_agent_lifecycle::TurnStartInput::new(
            super::super::PromptOrigin::from_prompt_id(prompt_id).is_synthetic(),
        );
        for contributor in self.extension_registry.turn_lifecycle_contributors() {
            contributor.on_turn_start(&turn_start_input).await;
        }
        if let Ok(mut pending) = self.rewind_pending_prompt.lock()
            && let Some(prev_text) = pending.take()
        {
            let new_text = prompt_blocks.iter().fold(String::new(), |mut acc, b| {
                if let acp::ContentBlock::Text(t) = b {
                    acc.push_str(&t.text);
                }
                acc
            });
            if new_text.trim() == prev_text.trim() {
                self.signals_handle().record_regeneration();
            } else {
                self.signals_handle().record_edit_and_retry();
            }
        }
        if let Some(bash_command) = Self::extract_bash_command(&prompt_blocks) {
            return self
                .handle_direct_bash_command(prompt_id, bash_command, &prompt_blocks)
                .await;
        }
        let slash_skills = self
            .agent
            .borrow()
            .tool_bridge()
            .clone()
            .slash_skills()
            .await;
        let skill_rewrite = if crate::session::is_cursor_user_template(
            &self.agent.borrow().definition().user_message_template,
        ) {
            slash_commands::SkillSlashRewrite::Passthrough
        } else {
            slash_commands::SkillSlashRewrite::RewriteToRun
        };
        let availability = self.command_availability().await;
        let mut pending_skill_information: Option<String> = None;
        let (workflow_registry, named_workflows) = self.named_workflow_snapshot();
        let original_prompt_text = prompt_blocks.iter().fold(String::new(), |mut acc, b| {
            if let acp::ContentBlock::Text(t) = b {
                acc.push_str(&t.text);
            }
            acc
        });
        let prompt_blocks = match slash_commands::resolve(
            prompt_blocks,
            &slash_skills,
            availability,
            skill_rewrite,
            &named_workflows,
        ) {
            Ok(blocks) => blocks,
            Err(SlashCommandOutcome::Builtin(action)) => {
                let text_block =
                    |text: String| acp::ContentBlock::Text(acp::TextContent::new(text));
                let slash_used = xai_grok_telemetry::events::SlashCommandUsed {
                    command: action.command_name().to_string(),
                    args_provided: action.args_provided(),
                };
                {
                    let span = tracing::Span::current();
                    span.record("command_name", action.command_name());
                    span.record("command_source", "builtin");
                }
                match action {
                    BuiltinAction::GoalSet {
                        objective,
                        token_budget,
                    } => {
                        xai_grok_telemetry::session_ctx::log_event(slash_used);
                        let reminder = self.setup_goal(&objective, token_budget).await;
                        vec![text_block(reminder)]
                    }
                    BuiltinAction::GoalResume => {
                        xai_grok_telemetry::session_ctx::log_event(slash_used);
                        match self.resume_goal().await {
                            GoalResumeOutcome::Inference { reminder, user_msg } => {
                                self.send_slash_command_output(&user_msg).await;
                                vec![text_block(reminder)]
                            }
                            GoalResumeOutcome::Message(msg) => {
                                self.persist_host_turn_user_echo(&original_prompt_text, prompt_id);
                                self.send_host_turn_slash_command_output(&msg).await;
                                return ok_end_turn(0, None);
                            }
                        }
                    }
                    BuiltinAction::WorkflowLaunch { name, input } => {
                        self.persist_host_turn_user_echo(&original_prompt_text, prompt_id);
                        let msg = self
                            .launch_named_workflow(&workflow_registry, &name, &input)
                            .await;
                        self.send_host_turn_slash_command_output(&msg).await;
                        return ok_end_turn(0, None);
                    }
                    _ => {
                        self.persist_host_turn_user_echo(&original_prompt_text, prompt_id);
                        return self.execute_builtin_slash_command(action).await;
                    }
                }
            }
            Err(SlashCommandOutcome::InvokeSkill {
                blocks: original_blocks,
                skills: parsed_skills,
            }) => {
                if let Some(first) = parsed_skills.first() {
                    *self.active_skill.lock() = Some(first.name.clone());
                    let span = tracing::Span::current();
                    span.record("command_name", first.name.as_str());
                    span.record(
                        "command_source",
                        if first.plugin_name.is_some() {
                            "plugin"
                        } else {
                            "skill"
                        },
                    );
                }
                for sk in &parsed_skills {
                    xai_grok_telemetry::session_ctx::log_event(
                        xai_grok_telemetry::events::SlashCommandUsed {
                            command: sk.name.clone(),
                            args_provided: !sk.args.is_empty(),
                        },
                    );
                    xai_grok_telemetry::session_ctx::log_event(
                        xai_grok_telemetry::events::SkillDispatched {
                            skill_name: sk.name.clone(),
                            plugin_source: sk.plugin_name.clone(),
                        },
                    );
                    let skill_source = if sk.plugin_name.is_some() {
                        "plugin"
                    } else {
                        crate::session::telemetry::skill_source_label(
                            &sk.skill_path,
                            self.session_info.cwd.as_str(),
                        )
                    };
                    tracing::info_span!(
                        "skill.activated",
                        skill_name = %sk.name,
                        invocation_trigger = "slash_command",
                        skill_source = skill_source,
                    )
                    .in_scope(|| {});
                    if let Some(ref pname) = sk.plugin_name {
                        xai_grok_telemetry::session_ctx::log_event(
                            xai_grok_telemetry::events::PluginUsed {
                                plugin_id: pname.clone(),
                                plugin_name: pname.clone(),
                                skill_name: Some(sk.name.clone()),
                                hook_event: None,
                                success: true,
                            },
                        );
                        tracing::info_span!(
                            "plugin.used",
                            plugin_name = %pname,
                            skill_name = %sk.name,
                        )
                        .in_scope(|| {});
                    }
                }
                pending_skill_information = slash_commands::build_skill_information_for_refs(
                    &parsed_skills,
                    &slash_skills,
                    &self.session_id_string(),
                )
                .await;
                original_blocks
            }
        };
        self.events.begin_turn();
        let model_id = self.current_model_id().await;
        let turn_number = self.chat_state_handle.get_prompt_index().await as u64;
        self.current_turn_number.set(turn_number);
        let yolo_mode = self.permissions.is_yolo_mode();
        let msg_count = self.chat_state_handle.get_conversation_len().await;
        let redirect_kind = if matches!(
            super::super::PromptOrigin::from_prompt_id(prompt_id),
            super::super::PromptOrigin::User
        ) {
            self.events.take_prior_redirect_kind()
        } else {
            None
        };
        self.emit_event(crate::session::events::Event::TurnStarted {
            session_id: self.session_id_string(),
            turn_number,
            model_id: model_id.clone(),
            yolo_mode,
            conversation_message_count: msg_count,
            session_relationship: crate::session::events::SessionRelationship::Primary,
            schema_version: crate::session::events::EVENT_SCHEMA_VERSION.into(),
            redirect_kind,
        });
        self.observability_bridge
            .emit(
                xai_tool_protocol::session_event::SessionEvent::TurnStarted {
                    turn_number,
                    model_id: model_id.clone(),
                    yolo_mode,
                },
            )
            .await;
        self.send_before_turn_event(xai_tool_protocol::turn_hook::BeforeTurnPayload {
            turn_number: self.chat_state_handle.get_prompt_index().await as u64,
            model_id: model_id.clone(),
            yolo_mode: self.permissions.is_yolo_mode(),
            conversation_message_count: msg_count,
            session_relationship: xai_tool_protocol::turn_hook::DEFAULT_SESSION_RELATIONSHIP
                .to_string(),
            schema_version: crate::session::events::EVENT_SCHEMA_VERSION.to_string(),
        })
        .await;
        let turn_idx = self.chat_state_handle.get_prompt_index().await as u64;
        xai_grok_telemetry::session_ctx::log_session_event(crate::agent::session_metrics::Turn {
            session_id: self.session_info.id.0.to_string(),
            turn_number: turn_idx,
        });
        let current_prompt_index = self.chat_state_handle.get_prompt_index().await;
        xai_grok_telemetry::session_ctx::begin_prompt_id();
        let origin = super::super::PromptOrigin::from_prompt_id(prompt_id);
        let mut chunk_meta = serde_json::Map::new();
        chunk_meta.insert("modelId".into(), serde_json::json!(model_id));
        chunk_meta.insert(
            "promptIndex".into(),
            serde_json::json!(current_prompt_index),
        );
        if origin.hide_user_echo_from_scrollback() {
            chunk_meta.insert("hideFromScrollback".into(), serde_json::json!(true));
        }
        let user_chunk_meta = Some(chunk_meta);
        self.chat_state_handle.increment_prompt_index();
        let text = prompt_blocks.iter().fold(String::new(), |mut acc, b| {
            if let acp::ContentBlock::Text(t) = b {
                acc.push_str(&t.text);
            }
            acc
        });
        let trimmed = text.trim().to_string();
        if !trimmed.is_empty() {
            self.chat_state_handle.cache_prompt_text(trimmed);
        }
        *self.tool_context.prompt_index.lock().await = current_prompt_index;
        self.file_state_tracker
            .begin_prompt(current_prompt_index)
            .await;
        let echo_mode = user_echo_mode(prompt_id);
        for block in prompt_blocks.iter() {
            let update = acp::SessionUpdate::UserMessageChunk(
                acp::ContentChunk::new(block.clone()).meta(user_chunk_meta.clone()),
            );
            let notification_meta = self.build_notification_meta();
            let notification = acp::SessionNotification::new(self.session_info.id.clone(), update)
                .meta(notification_meta.as_object().cloned());
            if echo_mode == UserEchoMode::PersistOnly {
                let _ = self
                    .notifications
                    .persistence_tx
                    .send(PersistenceMsg::Update(
                        crate::session::storage::SessionUpdate::Acp(Box::new(notification)),
                    ));
            } else {
                self.emit_notification_direct(notification).await;
            }
        }
        let crate::session::prompt_parser::ParsedPrompt {
            mut context,
            query,
            skill_information: skill_info,
            images: mut raw_images,
            is_cursor,
        } = match parse_prompt_with_skills(
            &prompt_blocks,
            self.tool_context.cwd.to_path_buf(),
            &self.session_info,
            verbatim,
            self.is_cursor_harness(),
            pending_skill_information.take().unwrap_or_default(),
        )
        .await
        {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!("Invalid prompt: {}", err.message);
                return Err(err);
            }
        };
        let recovered = crate::session::placeholder_images::recover_orphan_placeholders(
            &query,
            &mut raw_images,
            std::path::Path::new(&self.session_info.cwd),
        );
        if recovered > 0 {
            tracing::info!(
                session_id = %self.session_info.id,
                recovered,
                "server-side placeholder fallback: loaded orphan image(s) from disk",
            );
        }
        let query = crate::session::placeholder_images::strip_paths_from_image_placeholders(query);
        let user_images = self
            .normalize_images_with_notices(&mut context, raw_images, is_cursor)
            .await;
        let (query, extra_images) = if !self.is_cursor_harness() {
            let extraction = xai_grok_tools::util::base64_images::extract_base64_images(query);
            if extraction.images.is_empty() {
                (extraction.text, Vec::new())
            } else {
                let cleaned_text = extraction.text;
                let count = extraction.images.len();
                tracing::info!(
                    session_id = %self.session_info.id,
                    count,
                    "base64 images extracted from user query",
                );
                let acp_imgs: Vec<agent_client_protocol::ImageContent> = extraction
                    .images
                    .into_iter()
                    .map(|img| agent_client_protocol::ImageContent::new(img.data, img.mime_type))
                    .collect();
                let nr = crate::session::image_normalize::normalize_images(acp_imgs, false).await;
                if !nr.re_encode_fallbacks.is_empty() {
                    tracing::warn!(
                        session_id = %self.session_info.id,
                        notes = %nr.re_encode_fallbacks.join(" "),
                        "Extracted user query image kept original after re-encode failure",
                    );
                }
                (cleaned_text, nr.images)
            }
        } else {
            (query, Vec::new())
        };
        let assembled = crate::session::prompt_parser::ParsedPrompt::assemble_parts_with_skills(
            &context,
            &query,
            &skill_info,
            is_cursor,
        );
        let pre_truncation_text = assembled.clone();
        let (user_message, truncated_local_path) = if verbatim {
            (assembled, None)
        } else {
            self.maybe_truncate_large_prompt_with_skills(
                context,
                query,
                skill_info,
                is_cursor,
                current_prompt_index,
            )
            .await
        };
        let was_truncated = truncated_local_path.is_some();
        if let Some(tx) = parsed_prompt_tx {
            let _ = tx.send(ParsedPromptInfo {
                text: user_message.clone(),
                full_text: if was_truncated {
                    Some(pre_truncation_text)
                } else {
                    None
                },
                local_path: truncated_local_path,
            });
        }
        let _ = self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::ContentChunk(PersistenceContentChunk::new(
                prompt_blocks.to_vec(),
            )));
        let model_id = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| c.model)
            .unwrap_or_default();
        if self.telemetry_enabled || xai_grok_telemetry::external::is_active() {
            let effective_client_identifier =
                prompt_client_identifier.or_else(|| self.client_identifier.clone());
            let ev = xai_grok_telemetry::events::PromptSubmitted {
                prompt_length: user_message.len(),
                model_id,
                client_identifier: effective_client_identifier,
                screen_mode: prompt_screen_mode,
                prompt_text: None,
            };
            xai_grok_telemetry::session_ctx::log_event_dual(self.telemetry_enabled, ev);
        }
        self.maybe_inject_mcp_reminder().await;
        self.maybe_inject_mcp_connecting_reminder().await;
        self.maybe_inject_date_rollover_reminder().await;
        self.inject_plan_mode_reminders().await;
        self.inject_resumed_tasks_reminder();
        if matches!(&origin, super::super::PromptOrigin::User) {
            if let Some(gate) = &self.tool_context.task_wake_suppressed {
                gate.set(false);
            }
            xai_grok_telemetry::unified_log::info(
                "shell.task_wake.gate_cleared",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({ "reason": "handle_prompt_user_start" })),
            );
            self.consume_deferred_completions_for_user_turn().await;
        }
        self.drain_between_turn_completions().await;
        self.inject_workflow_status_reminder().await;
        let user_message = if user_images.is_empty() {
            user_message
        } else if self.is_cursor_harness() {
            self.transcribe_user_images(user_message, &user_images)
                .await?
        } else {
            let session_dir =
                crate::session::persistence::session_dir(&crate::session::info::Info {
                    id: self.session_info.id.clone(),
                    cwd: self.session_info.cwd.clone(),
                });
            crate::session::image_describe::persist_and_prepend_image_files(
                &session_dir,
                &user_images,
                &user_message,
            )
            .map_err(|e| {
                acp::Error::internal_error()
                    .data(format!("failed to save user images to assets dir: {e}"))
            })?
        };
        let attached_image_refs = if self.is_cursor_harness() {
            Vec::new()
        } else {
            crate::session::placeholder_images::attached_image_references(&user_images)
        };
        self.tool_bridge_handle()
            .update_resource(xai_grok_tools::types::resources::AttachedImages(
                attached_image_refs,
            ))
            .await;
        let prompt_text_for_hook = user_message.clone();
        {
            if trace_gcs_config.is_some() {
                self.chat_state_handle.begin_turn_capture();
            }
            let origin = super::super::PromptOrigin::from_prompt_id(prompt_id);
            if matches!(origin, super::super::PromptOrigin::User) {
                self.maybe_inject_interrupt_reminder().await;
            }
            let mut user_chat = match &origin {
                super::super::PromptOrigin::TaskCompleted { .. } => {
                    ConversationItem::task_completed(user_message)
                }
                super::super::PromptOrigin::SubagentCompleted { .. } => {
                    ConversationItem::subagent_completed(user_message)
                }
                super::super::PromptOrigin::WorkflowCompleted { .. } => {
                    ConversationItem::notification_drain(user_message)
                }
                super::super::PromptOrigin::NotificationDrain => {
                    ConversationItem::notification_drain(user_message)
                }
                super::super::PromptOrigin::GoalSummary => {
                    ConversationItem::goal_summary(user_message)
                }
                super::super::PromptOrigin::GoalClassifierNudge => {
                    ConversationItem::goal_classifier_nudge(user_message)
                }
                super::super::PromptOrigin::SchedulerFired => {
                    ConversationItem::scheduler_fired(user_message)
                }
                super::super::PromptOrigin::PlanResume => ConversationItem::user(user_message),
                super::super::PromptOrigin::User => {
                    let mut item = ConversationItem::user(user_message);
                    if let Some(interrupt) = self
                        .events
                        .take_prior_interrupt_category()
                        .and_then(crate::session::events::prior_turn_interrupt_from_cancellation)
                    {
                        item.set_prior_turn_interrupt(interrupt);
                    }
                    item
                }
            };
            user_chat.set_prompt_index(current_prompt_index);
            if !self.is_cursor_harness() {
                for image in &user_images {
                    user_chat.add_image(pick_user_image_url(image));
                }
                for image in &extra_images {
                    user_chat.add_image(format!("data:{};base64,{}", image.mime_type, image.data));
                }
            }
            if let Some(ack) = persist_ack {
                if self
                    .chat_state_handle
                    .push_user_message_and_ack(user_chat)
                    .await
                    .is_some()
                {
                    let (flush_tx, flush_rx) = oneshot::channel();
                    if self
                        .notifications
                        .persistence_tx
                        .send(PersistenceMsg::FlushAndAck {
                            respond_to: flush_tx,
                        })
                        .is_ok()
                        && flush_rx.await.is_ok()
                    {
                        let _ = ack.send(());
                    } else {
                        tracing::error!(
                            session_id = %self.session_info.id.0,
                            prompt_id = %prompt_id,
                            "persist_ack flush barrier failed"
                        );
                    }
                } else {
                    tracing::error!(
                        session_id = %self.session_info.id.0,
                        prompt_id = %prompt_id,
                        "persist_ack skipped: chat-state actor unavailable"
                    );
                }
            } else {
                self.chat_state_handle.push_user_message(user_chat);
            }
        }
        self.dispatch_hook(
            xai_grok_hooks::event::HookEventName::UserPromptSubmit,
            xai_grok_hooks::event::HookPayload::UserPromptSubmit {
                prompt: Some(prompt_text_for_hook),
            },
            Some(prompt_id),
            None,
        )
        .await;
        let turn_scope_guard =
            TurnSubagentScopeGuard::new(self.current_prompt_id.clone(), prompt_id.to_string());
        let turn_model_id = self.current_model_id().await;
        let doom_event_model = turn_model_id.clone();
        let turn_timer = std::time::Instant::now();
        let result = {
            let mut round_trace = trace_gcs_config;
            let mut round_artifact = artifact_tracker;
            let mut stop_continuations_this_turn: u32 = 0;
            loop {
                if self.goal_harness_enabled() {
                    let goal_loop_active = self.goal_tracker.lock().status()
                        == Some(crate::session::goal_tracker::GoalStatus::Active);
                    self.set_goal_loop_active_resource(goal_loop_active).await;
                }
                let round = self
                    .process_conversation_turn_with_recovery(
                        prompt_id,
                        round_trace.take(),
                        round_artifact.take(),
                        json_schema.clone(),
                    )
                    .await;
                if !matches!(round, Ok(TurnOutcome::Completed { .. })) {
                    break round;
                }
                if matches!(
                    round,
                    Ok(TurnOutcome::Completed {
                        refusal: Some(_),
                        ..
                    })
                ) {
                    self.auto_pause_goal_if_active_with_message(
                        crate::session::goal_tracker::GoalPauseReason::Infra,
                        "The model provider refused this goal round. Use /goal resume to retry."
                            .to_string(),
                    )
                    .await;
                    break round;
                }
                let goal_active = laziness_injection_active(
                    self.goal_harness_enabled(),
                    self.goal_tracker.lock().status(),
                );
                if goal_active {
                    let decision = if self.goal_runs_on_workflow_engine() {
                        self.run_goal_round_end().await
                    } else {
                        self.run_goal_round_end_legacy().await
                    };
                    if let GoalRoundDecision::Continue(directive) = decision {
                        self.inject_goal_continuation_message(directive).await;
                        continue;
                    }
                }
                match self
                    .run_stop_gate(prompt_id, stop_continuations_this_turn)
                    .await
                {
                    StopGateDecision::AllowStop => break round,
                    StopGateDecision::KeepWorking { feedback } => {
                        stop_continuations_this_turn += 1;
                        self.chat_state_handle
                            .push_user_message(ConversationItem::stop_hook_feedback(feedback));
                    }
                }
            }
        };
        let turn_duration_ms = turn_timer.elapsed().as_millis() as u64;
        let handle_prompt_elapsed_ms = handle_prompt_start.elapsed().as_millis() as u64;
        xai_grok_telemetry::unified_log::info(
            "shell.handle_prompt.done",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "prompt_id": prompt_id,
                "total_elapsed_ms": handle_prompt_elapsed_ms,
                "turn_elapsed_ms": turn_duration_ms,
                "pre_turn_ms": handle_prompt_elapsed_ms.saturating_sub(turn_duration_ms),
                "ok": result.is_ok(),
            })),
        );
        let turn_tool_count = self.events.tool_count_this_turn();
        let bridge_outcome = turn_result_to_hook_outcome(&result);
        self.observability_bridge
            .emit(xai_tool_protocol::session_event::SessionEvent::TurnEnded {
                turn_number: current_prompt_index as u64,
                outcome: bridge_outcome,
                duration_ms: turn_duration_ms,
                tool_call_count: turn_tool_count,
                model_id: turn_model_id.clone(),
            })
            .await;
        match &result {
            Ok(TurnOutcome::Completed { refusal, .. }) => {
                self.emit_turn_ended(
                    crate::session::events::TurnOutcomeLabel::Completed,
                    None,
                    None,
                );
                if let Some(explanation) = refusal {
                    let details = (!explanation.is_empty()).then(|| explanation.clone());
                    self.dispatch_hook(
                        xai_grok_hooks::event::HookEventName::StopFailure,
                        xai_grok_hooks::event::HookPayload::StopFailure {
                            error: xai_grok_hooks::event::StopFailureKind::InvalidRequest,
                            error_details: details.clone(),
                            last_assistant_message: details,
                        },
                        Some(prompt_id),
                        None,
                    )
                    .await;
                }
                self.send_after_turn_event(xai_tool_protocol::turn_hook::AfterTurnPayload {
                    turn_number: current_prompt_index as u64,
                    outcome: xai_tool_protocol::turn_hook::TurnHookOutcome::Completed,
                    duration_ms: turn_duration_ms,
                    tool_call_count: turn_tool_count,
                    model_id: turn_model_id.clone(),
                    written_repo_paths: Vec::new(),
                    cancellation_category: None,
                    cancellation_context: None,
                })
                .await;
                xai_grok_telemetry::session_ctx::log_event(
                    xai_grok_telemetry::events::TurnCompleted {
                        outcome: xai_grok_telemetry::events::Outcome::Completed,
                        duration_ms: turn_duration_ms,
                        tool_call_count: turn_tool_count,
                        model_id: turn_model_id,
                        cancellation_category: None,
                        error_category: None,
                    },
                );
            }
            Ok(TurnOutcome::Cancelled { category, context }) => {
                self.emit_turn_ended(
                    crate::session::events::TurnOutcomeLabel::Cancelled,
                    *category,
                    context.clone(),
                );
                if let Some(cause) = category {
                    self.events.set_prior_interrupt_category(*cause);
                }
                self.send_after_turn_event(xai_tool_protocol::turn_hook::AfterTurnPayload {
                    turn_number: current_prompt_index as u64,
                    outcome: xai_tool_protocol::turn_hook::TurnHookOutcome::Cancelled,
                    duration_ms: turn_duration_ms,
                    tool_call_count: turn_tool_count,
                    model_id: turn_model_id.clone(),
                    written_repo_paths: Vec::new(),
                    cancellation_category: cancellation_category_to_wire_string(*category),
                    cancellation_context: context.clone(),
                })
                .await;
                xai_grok_telemetry::session_ctx::log_event(
                    xai_grok_telemetry::events::TurnCompleted {
                        outcome: xai_grok_telemetry::events::Outcome::Cancelled,
                        duration_ms: turn_duration_ms,
                        tool_call_count: turn_tool_count,
                        model_id: turn_model_id,
                        cancellation_category: category.map(|c| format!("{c:?}")),
                        error_category: None,
                    },
                );
            }
            Ok(TurnOutcome::MaxTurnsReached { limit }) => {
                tracing::info!(limit, "turn ended: max_turns reached");
                self.emit_turn_ended(
                    crate::session::events::TurnOutcomeLabel::Cancelled,
                    None,
                    Some(serde_json::json!({
                        "reason": "max_turns_reached",
                        "limit": limit,
                    })),
                );
                self.send_after_turn_event(xai_tool_protocol::turn_hook::AfterTurnPayload {
                    turn_number: current_prompt_index as u64,
                    outcome: xai_tool_protocol::turn_hook::TurnHookOutcome::Cancelled,
                    duration_ms: turn_duration_ms,
                    tool_call_count: turn_tool_count,
                    model_id: turn_model_id.clone(),
                    written_repo_paths: Vec::new(),
                    cancellation_category: None,
                    cancellation_context: Some(serde_json::json!({
                        "reason": "max_turns_reached",
                        "limit": limit,
                    })),
                })
                .await;
                xai_grok_telemetry::session_ctx::log_event(
                    xai_grok_telemetry::events::TurnCompleted {
                        outcome: xai_grok_telemetry::events::Outcome::Cancelled,
                        duration_ms: turn_duration_ms,
                        tool_call_count: turn_tool_count,
                        model_id: turn_model_id,
                        cancellation_category: Some("max_turns_reached".to_string()),
                        error_category: None,
                    },
                );
            }
            Err(err) => {
                self.emit_turn_ended(crate::session::events::TurnOutcomeLabel::Error, None, None);
                self.send_after_turn_event(xai_tool_protocol::turn_hook::AfterTurnPayload {
                    turn_number: current_prompt_index as u64,
                    outcome: xai_tool_protocol::turn_hook::TurnHookOutcome::Error,
                    duration_ms: turn_duration_ms,
                    tool_call_count: turn_tool_count,
                    model_id: turn_model_id.clone(),
                    written_repo_paths: Vec::new(),
                    cancellation_category: None,
                    cancellation_context: None,
                })
                .await;
                let error_category = Self::classify_turn_error(err);
                xai_grok_telemetry::session_ctx::log_session_event(
                    xai_grok_telemetry::events::ApiError {
                        error_category: error_category.clone(),
                        model_id: turn_model_id.clone(),
                        status_code: None,
                        duration_ms: Some(turn_duration_ms),
                    },
                );
                xai_grok_telemetry::session_ctx::log_event(
                    xai_grok_telemetry::events::TurnCompleted {
                        outcome: xai_grok_telemetry::events::Outcome::Error,
                        duration_ms: turn_duration_ms,
                        tool_call_count: turn_tool_count,
                        model_id: turn_model_id,
                        cancellation_category: None,
                        error_category: Some(error_category),
                    },
                );
                self.dispatch_hook(
                    xai_grok_hooks::event::HookEventName::StopFailure,
                    xai_grok_hooks::event::HookPayload::StopFailure {
                        error: Self::stop_failure_error_type(err),
                        error_details: Self::turn_error_detail(err),
                        last_assistant_message: Some(Self::format_turn_error_message(err)),
                    },
                    Some(prompt_id),
                    None,
                )
                .await;
            }
        }
        xai_grok_telemetry::session_ctx::log_session_event(
            crate::agent::session_metrics::TurnCompletedLifecycle {
                session_id: self.session_info.id.0.to_string(),
                turn_number: current_prompt_index as u64,
            },
        );
        let doom_tally = std::mem::take(&mut *self.doom_loop_turn_tally.lock());
        if doom_tally.fired() {
            xai_grok_telemetry::session_ctx::log_session_event(
                crate::agent::session_metrics::DoomLoopRecovery {
                    session_id: self.session_info.id.0.to_string(),
                    turn_number: current_prompt_index as u64,
                    attempts: doom_tally.attempts,
                    accepted_after_budget: doom_tally.accepted_after_budget,
                    top_trigger: doom_tally.top_trigger,
                    model: doom_event_model,
                },
            );
        }
        match &result {
            Ok(TurnOutcome::Completed { .. }) => {
                for contributor in self.extension_registry.turn_lifecycle_contributors() {
                    contributor
                        .on_turn_done(&xai_agent_lifecycle::TurnDoneInput)
                        .await;
                }
            }
            Ok(TurnOutcome::Cancelled { .. }) | Ok(TurnOutcome::MaxTurnsReached { .. }) => {
                let input = xai_agent_lifecycle::TurnAbortInput::new(
                    xai_agent_lifecycle::TurnAbortReason::Interrupted,
                );
                for contributor in self.extension_registry.turn_lifecycle_contributors() {
                    contributor.on_turn_abort(&input).await;
                }
            }
            Err(err) => {
                let message = err.to_string();
                let input = xai_agent_lifecycle::TurnErrorInput { message: &message };
                for contributor in self.extension_registry.turn_lifecycle_contributors() {
                    contributor.on_turn_error(&input).await;
                }
            }
        }
        if matches!(
            result,
            Ok(TurnOutcome::Cancelled { .. }) | Ok(TurnOutcome::MaxTurnsReached { .. })
        ) {
            self.cancel_running_turn_subagents();
        }
        self.flush_to_disk().await;
        self.file_state_tracker
            .end_prompt(&self.tool_context.fs, current_prompt_index)
            .await;
        if let Some(mut rewind_point) = self
            .file_state_tracker
            .get_rewind_point(current_prompt_index)
            .await
        {
            rewind_point.normalize_to_relative(self.tool_context.cwd.as_ref());
            let _ = self
                .notifications
                .persistence_tx
                .send(PersistenceMsg::RewindPoint(rewind_point));
        }
        match result {
            Ok(outcome) => {
                let usage = self.freeze_prompt_usage(prompt_id).await;
                drop(turn_scope_guard);
                self.chat_state_handle.flush();
                let total_tokens = self.chat_state_handle.get_total_tokens().await;
                let (stop_reason, mut snapshot, completion_kind, structured_output) = match outcome
                {
                    TurnOutcome::Completed {
                        snapshot,
                        structured_output,
                        refusal,
                        ..
                    } => (
                        if refusal.is_some() {
                            acp::StopReason::Refusal
                        } else {
                            acp::StopReason::EndTurn
                        },
                        *snapshot,
                        PromptCompletionKind::Completed,
                        structured_output,
                    ),
                    TurnOutcome::Cancelled { category, context } => {
                        let cancellation_ctx = context.and_then(|v| serde_json::from_value(v).ok());
                        (
                            acp::StopReason::Cancelled,
                            None,
                            PromptCompletionKind::Cancelled {
                                category,
                                context: cancellation_ctx,
                            },
                            None,
                        )
                    }
                    TurnOutcome::MaxTurnsReached { limit } => (
                        acp::StopReason::Cancelled,
                        None,
                        PromptCompletionKind::MaxTurnsReached { limit },
                        None,
                    ),
                };
                if let Some(snapshot) = snapshot.as_mut() {
                    self.apply_prompt_modes_to_snapshot(snapshot);
                }
                Ok(crate::session::commands::PromptTurnOk {
                    stop_reason,
                    total_tokens,
                    turn_snapshot: snapshot,
                    completion_kind,
                    structured_output,
                    usage,
                    tool_overrides: None,
                })
            }
            Err(e) => {
                let usage = self.freeze_prompt_usage(prompt_id).await;
                drop(turn_scope_guard);
                Err(crate::sampling::error::attach_prompt_usage(e, usage))
            }
        }
    }
    /// Wait for turn-blocking subagents (up to 120s on the turn task),
    /// snapshot, clear sticky. Background children never gate the drain: the
    /// prompt report is marked incomplete immediately and their spend reaches
    /// the session ledger when they finish.
    /// Cancel intentionally skips this multi-second drain (actor-loop safety).
    pub(super) async fn freeze_prompt_usage(
        &self,
        prompt_id: &str,
    ) -> Option<crate::extensions::notification::PromptUsage> {
        const DRAIN: std::time::Duration = std::time::Duration::from_secs(120);
        self.freeze_prompt_usage_bounded(prompt_id, DRAIN).await
    }
    /// [`freeze_prompt_usage`] with an explicit drain bound, for tests.
    pub(super) async fn freeze_prompt_usage_bounded(
        &self,
        prompt_id: &str,
        max_wait: std::time::Duration,
    ) -> Option<crate::extensions::notification::PromptUsage> {
        let drain = self
            .drain_subagent_usage_for_prompt_bounded(prompt_id, max_wait)
            .await;
        self.finalize_usage_from_outcome(prompt_id, drain).await
    }
    /// Waits for turn-blocking folds only.
    /// `fail_closed` on timeout or query failure; sticky and `background_live`
    /// are report-level only (no ledger mark). Must run on the turn task (not
    /// the session actor loop) so folds can land.
    pub(super) async fn drain_subagent_usage_for_prompt_bounded(
        &self,
        prompt_id: &str,
        max_wait: std::time::Duration,
    ) -> UsageDrainOutcome {
        const POLL: std::time::Duration = std::time::Duration::from_millis(50);
        let deadline = std::time::Instant::now() + max_wait;
        loop {
            let reply = self.outstanding_reply_for_prompt(prompt_id).await;
            match reply.as_ref() {
                None => {
                    tracing::warn!(
                        prompt_id,
                        "outstanding subagent query failed; treating usage as incomplete"
                    );
                    return UsageDrainOutcome {
                        fail_closed: true,
                        background_live: false,
                        sticky_report: false,
                    };
                }
                Some(r) if r.live_ids.is_empty() => {
                    return UsageDrainOutcome {
                        fail_closed: false,
                        background_live: r.background_live,
                        sticky_report: r.subagent_usage_not_applied,
                    };
                }
                Some(r) => {
                    if std::time::Instant::now() >= deadline {
                        tracing::warn!(
                            prompt_id,
                            count = r.live_ids.len(),
                            max_wait_ms = max_wait.as_millis() as u64,
                            "subagent usage drain timed out; usage may under-count"
                        );
                        return UsageDrainOutcome {
                            fail_closed: true,
                            background_live: r.background_live,
                            sticky_report: r.subagent_usage_not_applied,
                        };
                    }
                }
            }
            tokio::time::sleep(POLL).await;
        }
    }
    pub(super) async fn snapshot_prompt_usage(
        &self,
    ) -> Option<crate::extensions::notification::PromptUsage> {
        self.snapshot_prompt_usage_marked(false).await
    }
    pub(super) async fn snapshot_prompt_usage_marked(
        &self,
        incomplete: bool,
    ) -> Option<crate::extensions::notification::PromptUsage> {
        let actor_background_spend = self
            .unattributed_background_usage
            .swap(false, std::sync::atomic::Ordering::Relaxed);
        let shared_background_spend = self
            .tool_context
            .unattributed_background_usage
            .swap(false, std::sync::atomic::Ordering::Relaxed);
        let incomplete = incomplete || actor_background_spend || shared_background_spend;
        match self.chat_state_handle.try_get_prompt_usage().await {
            Ok(ledger) => {
                let incomplete = incomplete || ledger.as_ref().is_some_and(|l| l.incomplete);
                crate::extensions::notification::PromptUsage::project_from_ledger(
                    ledger.as_ref(),
                    incomplete,
                )
            }
            Err(()) => {
                crate::extensions::notification::PromptUsage::project_from_ledger(None, true)
            }
        }
    }
    /// When freeze did not attach: incomplete if billed or may under-count; else omit.
    pub(super) async fn error_path_usage_fallback(
        &self,
        prompt_id: &str,
    ) -> Option<crate::extensions::notification::PromptUsage> {
        let may_undercount = Self::usage_incomplete_from_reply(
            self.outstanding_reply_for_prompt(prompt_id).await.as_ref(),
        );
        match self.chat_state_handle.try_get_prompt_usage().await {
            Ok(ledger) => crate::extensions::notification::PromptUsage::for_error_path(
                ledger.as_ref(),
                may_undercount,
            ),
            Err(()) => crate::extensions::notification::PromptUsage::for_error_path(None, true),
        }
    }
    /// Sticky incomplete for `prompt_id`, or the live pin when `None`.
    /// Returns true only if the coordinator acked the mark.
    pub(super) async fn mark_subagent_usage_not_applied(&self, prompt_id: Option<&str>) -> bool {
        let resolved = prompt_id
            .map(str::to_owned)
            .or_else(|| self.current_prompt_id.lock().ok().and_then(|g| g.clone()));
        let Some(pid) = resolved else {
            self.unattributed_background_usage
                .store(true, std::sync::atomic::Ordering::Relaxed);
            self.tool_context
                .unattributed_background_usage
                .store(true, std::sync::atomic::Ordering::Relaxed);
            return false;
        };
        let Some(tx) = &self.tool_context.subagent_event_tx else {
            return false;
        };
        use xai_grok_tools::implementations::grok_build::task::types::{
            SubagentEvent, SubagentMarkUsageNotAppliedRequest,
        };
        let (respond_to, ack) = tokio::sync::oneshot::channel();
        if tx
            .send(SubagentEvent::MarkUsageNotApplied(
                SubagentMarkUsageNotAppliedRequest {
                    prompt_id: pid,
                    respond_to,
                },
            ))
            .is_err()
        {
            return false;
        }
        ack.await.is_ok()
    }
    /// Drain this session's buffered mid-turn monitor events
    /// (`drain_owned` — leader mode shares the buffer) into ONE hidden
    /// synthetic user message, tagged `SyntheticReason::SystemReminder` so
    /// compaction/fork/pruning skip it. Deliberately a bare
    /// `push_user_message`, NOT `inject_synthetic_user_message`: the latter
    /// persists a `UserMessageChunk` to `updates.jsonl`, which resume
    /// replays — the raw XML would render as a user prompt. Clients see
    /// monitor events only via the structured `x.ai/monitor_event` channel.
    pub(crate) async fn inject_pending_monitor_events(&self) {
        let Some(buffer) = &self.tool_context.monitor_event_buffer else {
            return;
        };
        let mine = xai_grok_tools::implementations::grok_build::task::types::drain_owned(
            buffer,
            Some(self.session_info.id.0.as_ref()),
        );
        if mine.is_empty() {
            return;
        }
        let Some(body) = xai_grok_tools::reminders::task_completion::format_monitor_events(
            &mine,
            Some(&self.tool_context.task_output_tool_name),
        ) else {
            return;
        };
        let wrapped = xai_grok_tools::reminders::wrap_reminder(&body);
        self.chat_state_handle
            .push_user_message(ConversationItem::system_reminder(wrapped));
        tracing::info!(
            session_id = %self.session_info.id.0,
            count = mine.len(),
            "injected mid-turn monitor events as hidden synthetic user message"
        );
    }
    /// Per-turn hook called from the event-loop completion handler
    /// after every turn finishes. Two terminal branches when the
    /// goal is `Active` (`goal_active_now == true`):
    ///
    /// 1. **Success.** Reset `goal_continuation_streak` to 0, then call
    ///    `maybe_queue_goal_continuation`. That helper verifies any
    ///    pending completion via its turn-end drain, queues the
    ///    continuation reminder if the goal is still `Active`, and runs
    ///    the stop-detector to select the nudge flavor (generic vs.
    ///    bail-specific) and emit `Event::GoalPrematureStopDetected`.
    /// 2. **Non-success.** Increment `goal_continuation_streak`. At
    ///    [`GOAL_CONTINUATION_BACKOFF_THRESHOLD`] consecutive hits,
    ///    reset the streak and auto-pause with
    ///    `GoalPauseReason::BackOff`. No continuation is queued on this path: an
    ///    infra-error / cancelled turn rarely carries a deliberate
    ///    turn-final message, and stop-detection lives on the success
    ///    path inside `maybe_queue_goal_continuation`.
    ///
    /// When the goal is not `Active` (`goal_active_now == false` —
    /// the doom-loop / infra-error branches in the event loop ran
    /// before this method and already transitioned the goal out of
    /// Active), both branches are skipped: neither streak moves and the
    /// existing pause cause is preserved.
    pub(crate) async fn handle_turn_end(&self, turn_succeeded: bool) {
        let goal_active_now = laziness_injection_active(
            self.goal_harness_enabled(),
            self.goal_tracker.lock().status(),
        );
        if turn_succeeded && goal_active_now {
            self.goal_continuation_streak
                .store(0, std::sync::atomic::Ordering::Relaxed);
            self.maybe_queue_goal_continuation().await;
            return;
        }
        if !turn_succeeded && goal_active_now {
            let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
            if self.enforce_goal_token_budget(current_tokens).await {
                return;
            }
            let streak = self
                .goal_continuation_streak
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            if streak >= GOAL_CONTINUATION_BACKOFF_THRESHOLD {
                self.goal_continuation_streak
                    .store(0, std::sync::atomic::Ordering::Relaxed);
                self.auto_pause_goal_if_active(
                    crate::session::goal_tracker::GoalPauseReason::BackOff,
                )
                .await;
                self.send_slash_command_output(&format!(
                    "Goal auto-paused after {GOAL_CONTINUATION_BACKOFF_THRESHOLD} consecutive \
                     non-completing turns. The model is not making progress. \
                     Use /goal resume to retry or /goal clear to abandon."
                ))
                .await;
            }
        }
    }
    /// Wraps `process_conversation_turn` with auto-recovery for agents that opt in.
    ///
    /// Agents with a `completion_requirement` in their definition require the model
    /// to call a specific tool before finishing. If a prompt turn ends without that
    /// tool having been called, this method injects the recovery prompt and re-runs
    /// the turn with exponential backoff.
    ///
    /// Agents without `completion_requirement` bypass this entirely.
    #[tracing::instrument(
        name = "session.process_conversation_turn_with_recovery",
        skip_all,
        err,
        fields(req_id = %req_id, session_id = %self.session_info.id.0)
    )]
    pub(super) async fn process_conversation_turn_with_recovery(
        self: &Arc<Self>,
        req_id: &str,
        trace_gcs_config: Option<crate::session::repo_changes::TraceExportConfig>,
        artifact_tracker: Option<crate::upload::manifest::ArtifactTracker>,
        json_schema: Option<serde_json::Value>,
    ) -> Result<TurnOutcome, acp::Error> {
        let _ = self.compaction.auto_compact_suppressed.compare_exchange(
            crate::session::compaction_config::SUPPRESS_TURN,
            crate::session::compaction_config::SUPPRESS_NONE,
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
        );
        let agent_ref = self.agent.borrow();
        let completion_req = match agent_ref.completion_requirement() {
            Some(req) => req,
            None => {
                return self
                    .process_conversation_turn(
                        req_id,
                        trace_gcs_config,
                        artifact_tracker.as_ref(),
                        json_schema,
                    )
                    .await;
            }
        };
        let recovery = match &completion_req.recovery {
            Some(r) => r.clone(),
            None => {
                return self
                    .process_conversation_turn(
                        req_id,
                        trace_gcs_config,
                        artifact_tracker.as_ref(),
                        json_schema,
                    )
                    .await;
            }
        };
        let required_tool = completion_req.tool.clone();
        let recovery_prompt = completion_req.reminder.clone();
        let mut result = self
            .process_conversation_turn(
                req_id,
                trace_gcs_config.clone(),
                artifact_tracker.as_ref(),
                json_schema.clone(),
            )
            .await;
        if matches!(result, Ok(TurnOutcome::MaxTurnsReached { .. })) {
            return result;
        }
        if let Ok(TurnOutcome::Completed {
            ref tools_called, ..
        }) = result
            && tools_called.iter().any(|name| name == &required_tool)
        {
            tracing::info!(
                "Completion requirement satisfied (tool '{}' called) for session {}",
                required_tool,
                self.session_info.id.0,
            );
            return result;
        }
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let error_desc = match &result {
                Ok(_) => "Agent finished without completing required task".into(),
                Err(e) => format!("{e:?}"),
            };
            if attempt > recovery.max_retries {
                tracing::error!(
                    "Auto-recovery exhausted after {attempt} attempts for session {}: {error_desc}",
                    self.session_info.id.0,
                );
                self.send_xai_notification(XaiSessionUpdate::AutoRecoveryExhausted {
                    attempts: attempt,
                    error: error_desc,
                })
                .await;
                return result;
            }
            let delay_ms = std::cmp::min(
                recovery.base_delay_ms * 2u64.pow(attempt.saturating_sub(1)),
                recovery.max_delay_ms,
            );
            let delay = std::time::Duration::from_millis(delay_ms);
            tracing::warn!(
                "Auto-recovery attempt {}/{} for session {}: {error_desc}. Retrying in {}ms",
                attempt,
                recovery.max_retries,
                self.session_info.id.0,
                delay.as_millis(),
            );
            self.send_xai_notification(XaiSessionUpdate::AutoRecoveryStarted {
                attempt,
                max_retries: recovery.max_retries,
                error: error_desc,
                delay_ms: delay.as_millis() as u64,
            })
            .await;
            sleep(delay).await;
            let recovery_message = ConversationItem::auto_recovery(recovery_prompt.clone());
            self.chat_state_handle.push_user_message(recovery_message);
            result = self
                .process_conversation_turn(
                    req_id,
                    trace_gcs_config.clone(),
                    artifact_tracker.as_ref(),
                    None,
                )
                .await;
            if matches!(result, Ok(TurnOutcome::MaxTurnsReached { .. })) {
                return result;
            }
            if let Ok(TurnOutcome::Completed {
                ref tools_called, ..
            }) = result
                && tools_called.iter().any(|name| name == &required_tool)
            {
                tracing::info!(
                    "Completion requirement satisfied after {} recovery attempt(s) \
                     (tool '{}' called) for session {}",
                    attempt,
                    required_tool,
                    self.session_info.id.0,
                );
                return result;
            }
        }
    }
    /// Compute the first-turn memory reminder, if one should be injected.
    ///
    /// A block persisted by an earlier session segment (a prior `--resume`
    /// process, or a turn before a compaction) is reused verbatim — see
    /// [`conversation_has_memory_context`] for why re-searching is harmful.
    ///
    /// [`conversation_has_memory_context`]: crate::session::helpers::memory_context::conversation_has_memory_context
    pub(crate) async fn first_turn_memory_reminder(&self) -> Option<String> {
        if self
            .memory
            .context_injected
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return None;
        }
        self.memory
            .context_injected
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if !self.memory.initial_injection_config.enabled {
            tracing::info!(
                target: xai_grok_telemetry::memory_log::TARGET,
                "MEMORY_INJECT: first-turn injection disabled by config"
            );
            return None;
        }
        let (Some(storage), Some(params)) =
            (self.memory.storage(), self.memory.backend_params.as_ref())
        else {
            return None;
        };
        let conversation = self.chat_state_handle.get_conversation().await;
        if crate::session::helpers::memory_context::conversation_has_memory_context(&conversation) {
            tracing::info!(
                target: xai_grok_telemetry::memory_log::TARGET,
                "MEMORY_INJECT: existing memory-context block present in system message -- skipping re-injection to preserve prompt cache"
            );
            return None;
        }
        use xai_grok_tools::types::memory_backend::MemoryBackend as _;
        let (injection_params, configured_min_score) =
            build_initial_injection_backend_params(params, &self.memory.initial_injection_config);
        let backend = crate::session::memory::MemoryBackendImpl::from_session_params(
            storage,
            &injection_params,
        );
        let raw_query =
            crate::session::helpers::session_compact::extract_last_real_user_query(&conversation)
                .unwrap_or_default();
        let was_greeting = raw_query.is_empty()
            || raw_query.len() < 20
            || crate::session::helpers::memory_context::is_greeting(&raw_query);
        let query = if was_greeting {
            "project conventions preferences architecture".to_string()
        } else {
            raw_query
        };
        let inject_start = std::time::Instant::now();
        let inject_results = backend.search(&query, 6, configured_min_score).await.ok();
        let result_count = inject_results.as_ref().map_or(0, |r| r.len());
        let top_score = inject_results
            .as_ref()
            .and_then(|r| r.first())
            .map_or(0.0, |r| r.score);
        let total_snippet_chars: usize = inject_results
            .as_ref()
            .map_or(0, |r| r.iter().map(|s| s.snippet.len()).sum());
        tracing::info!(
            target: xai_grok_telemetry::memory_log::TARGET,
            configured_min_score,
            "MEMORY_INJECT_SEARCH: results={result_count}"
        );
        xai_grok_telemetry::session_ctx::log_event(
            xai_grok_telemetry::memory_telemetry::MemoryInjection {
                session_id: self.session_info.id.to_string(),
                was_greeting_fallback: was_greeting,
                result_count,
                total_snippet_chars,
                top_score,
                configured_min_score,
                injection_duration_ms: inject_start.elapsed().as_millis() as u64,
            },
        );
        inject_results.and_then(|results| {
            crate::session::helpers::memory_context::format_memory_reminder(&results)
        })
    }
    /// Inspect `tool_calls` for a `StructuredOutput` call and decide the turn's
    /// next step, pushing the call's `tool_result` (correction / retry error /
    /// terminal) as a side effect. Validates the args against `validator` and
    /// bumps `retries` on a non-conforming retry.
    async fn handle_structured_output_tool_call(
        &self,
        tool_calls: &mut Vec<xai_grok_sampling_types::conversation::ToolCall>,
        validator: &Result<jsonschema::Validator, String>,
        retries: &mut u32,
    ) -> StructuredOutputStep {
        let Some(pos) = tool_calls
            .iter()
            .position(|tc| tc.name == STRUCTURED_OUTPUT_TOOL)
        else {
            return StructuredOutputStep::Proceed;
        };
        if tool_calls.len() > 1 {
            for tc in tool_calls
                .iter()
                .filter(|tc| tc.name == STRUCTURED_OUTPUT_TOOL)
            {
                self.chat_state_handle
                    .push_tool_result(ConversationItem::tool_result(
                        tc.id.as_ref().to_owned(),
                        "Call StructuredOutput alone, exactly once, after all other tools finish.",
                    ));
            }
            tool_calls.retain(|tc| tc.name != STRUCTURED_OUTPUT_TOOL);
            return StructuredOutputStep::Proceed;
        }
        let call_id = tool_calls[pos].id.as_ref().to_owned();
        let validated = validate_structured_output(validator, &tool_calls[pos].arguments);
        if let Err(err) = &validated
            && *retries < STRUCTURED_OUTPUT_MAX_RETRIES
        {
            *retries += 1;
            self.chat_state_handle
                .push_tool_result(ConversationItem::tool_result(
                    call_id,
                    format!("{err}\nFix the arguments and call StructuredOutput again."),
                ));
            return StructuredOutputStep::Retry;
        }
        self.chat_state_handle
            .push_tool_result(ConversationItem::tool_result(
                call_id,
                match &validated {
                    Ok(_) => "Structured output accepted.".to_string(),
                    Err(err) => err.clone(),
                },
            ));
        StructuredOutputStep::Complete(validated)
    }
    /// Shared turn-completion bookkeeping (plan cleanup, signals snapshot +
    /// persistence, BigQuery turn delta, feedback prompt). Runs identically for
    /// the native and StructuredOutput-tool completion paths. Returns the
    /// turn-end snapshot for `TurnOutcome::Completed`.
    async fn finalize_turn_bookkeeping(
        &self,
        req_id: &str,
        conv_turn_start: std::time::Instant,
        turn_span_totals: &TurnSpanTotals,
        model_fingerprint: Option<String>,
    ) -> Option<TurnDeltaSnapshot> {
        self.emit_turn_end_plan_cleanup().await;
        self.signals_handle().record_turn_complete();
        let mut snapshot = self.signals_handle().take_turn_end_snapshot().await;
        if let Some(snap) = snapshot.as_mut() {
            self.apply_prompt_modes_to_snapshot(snap);
            snap.turn_input_tokens = turn_span_totals.input_tokens.max(0) as u64;
            snap.turn_output_tokens = turn_span_totals.output_tokens.max(0) as u64;
            snap.turn_cached_input_tokens = turn_span_totals.cache_read_tokens.max(0) as u64;
            for pr in &snap.delta.prs_created_this_turn {
                xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::PrCreated {
                    source: pr.source,
                    had_commit_in_session: pr.had_commit_in_session,
                });
            }
        }
        if let Some(snap) = snapshot.as_ref() {
            let _ = self
                .notifications
                .persistence_tx
                .send(PersistenceMsg::Signals(snap.current.clone()));
        }
        self.feedback_manager
            .send_turn_delta_with_snapshot(
                snapshot.clone(),
                Some(req_id.to_string()),
                Some(conv_turn_start.elapsed().as_millis() as i64),
                Some("completed".to_string()),
                model_fingerprint,
            )
            .await;
        if let Some(request) = self
            .feedback_manager
            .maybe_request_feedback(Some(req_id.to_string()))
            .await
        {
            self.send_feedback_notification(request).await;
        }
        snapshot
    }
    #[tracing::instrument(
        name = "session.process_conversation_turn",
        skip_all,
        err,
        fields(
            session_id = %self.session_info.id.0,
            model_id,
            turn_tool_count,
            turn_model_calls,
            input_tokens = tracing::field::Empty,
            output_tokens = tracing::field::Empty,
            cache_read_tokens = tracing::field::Empty,
            stop_reason = tracing::field::Empty,
            response.has_tool_call = tracing::field::Empty,
            request_id = tracing::field::Empty,
            ttft_ms = tracing::field::Empty,
            mcp_server.name = tracing::field::Empty,
            mcp_tool.name = tracing::field::Empty,
            agent.name = tracing::field::Empty,
            skill.name = tracing::field::Empty,
            query_source = tracing::field::Empty,
            effort = tracing::field::Empty,
            attempt = tracing::field::Empty,
            parent_agent_id = tracing::field::Empty,
        )
    )]
    async fn process_conversation_turn(
        self: &Arc<Self>,
        req_id: &str,
        trace_gcs_config: Option<crate::session::repo_changes::TraceExportConfig>,
        artifact_tracker: Option<&crate::upload::manifest::ArtifactTracker>,
        json_schema: Option<serde_json::Value>,
    ) -> Result<TurnOutcome, acp::Error> {
        let conv_turn_start = std::time::Instant::now();
        self.maybe_refresh_model_metadata_on_resume().await;
        self.maybe_compact_on_model_switch().await?;
        self.chat_state_handle
            .record_turn_start(chrono::Utc::now().timestamp_millis());
        {
            let span = tracing::Span::current();
            if let Some(agent) = self.active_agent_type.lock().clone() {
                span.record("agent.name", agent.as_str());
            }
            if let Some(skill) = self.active_skill.lock().clone() {
                span.record("skill.name", skill.as_str());
            }
            span.record(
                "query_source",
                if self.startup_hints.is_subagent {
                    "subagent"
                } else {
                    "main"
                },
            );
            if let Some(parent) = self.startup_hints.parent_session_id.as_deref() {
                span.record("parent_agent_id", parent);
            }
        }
        if let Some(cfg) = self.chat_state_handle.get_sampling_config().await {
            let span = tracing::Span::current();
            span.record("model_id", cfg.model.as_str());
            if let Some(effort) = cfg.reasoning_effort {
                span.record("effort", effort.as_str());
            }
        }
        let mut prompt_timing = Some(crate::session::prompt_timing::PromptTiming::start());
        let tool_prep_start = std::time::Instant::now();
        let (tool_definitions, mcp_wait_ms) = self.prepare_tool_definitions_timed().await;
        let total_prep_ms = tool_prep_start.elapsed().as_millis() as u64;
        if let Some(ref mut pt) = prompt_timing {
            pt.record_tool_prep(mcp_wait_ms, total_prep_ms);
        }
        xai_grok_telemetry::unified_log::info(
            "shell.turn.tool_prep_done",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "tool_count": tool_definitions.len(),
                "mcp_wait_ms": mcp_wait_ms,
                "total_prep_ms": total_prep_ms,
                "elapsed_since_turn_start_ms": conv_turn_start.elapsed().as_millis() as u64,
            })),
        );
        if let Some(ref gcs_config) = trace_gcs_config {
            let gcs_cfg = gcs_config.clone();
            let tool_defs = tool_definitions.clone();
            let manifest_clone = artifact_tracker.cloned();
            let auth_manager = self.auth_manager.clone();
            tokio::spawn(async move {
                crate::upload::trace::upload_tool_definitions(
                    gcs_cfg,
                    auth_manager,
                    &tool_defs,
                    manifest_clone.as_ref(),
                )
                .await;
            });
        }
        self.record_turn_model().await;
        let mut metrics_drop_guard = TurnMetrics::new();
        let mut turn_tools_called: Vec<String> = Vec::new();
        let mut tool_turn_count: usize = 1;
        let mut loop_index: u32 = 0;
        let mut identical_tool_calls = IdenticalToolCallRun::default();
        let mut todo_gate_fires: u32 = 0;
        let mut auth_retry_schedule = AuthRetrySchedule::new();
        let mut turn_span_totals = TurnSpanTotals::default();
        let mut model_fingerprint: Option<String> = None;
        let mut structured_output_retries: u32 = 0;
        let structured_output_validator = json_schema.as_ref().map(|schema| {
            jsonschema::validator_for(schema).map_err(|e| format!("invalid output schema: {e}"))
        });
        let schema_ok = matches!(structured_output_validator, Some(Ok(_)));
        let native_backend = if json_schema.is_some() {
            match self.chat_state_handle.get_sampling_config().await {
                Some(c) => c.api_backend.supports_native_schema(),
                None => {
                    tracing::warn!(
                        "structured output: no sampling config; using StructuredOutput tool"
                    );
                    false
                }
            }
        } else {
            false
        };
        let structured_output_native = schema_ok && native_backend;
        let structured_output_tool = schema_ok && !native_backend;
        if structured_output_tool {
            self.push_system_reminder(
                "A response schema is required. After any tool use, call the \
                 `StructuredOutput` tool exactly once with your final answer as its \
                 arguments; do not return the answer as text.",
            );
        }
        loop {
            self.emit_event(crate::session::events::Event::LoopStarted { loop_index });
            loop_index += 1;
            if identical_tool_calls.run_len >= MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS {
                let run_len = identical_tool_calls.run_len;
                let tool_name = identical_tool_calls.tool_name.clone();
                tracing::warn!(
                    session_id = %self.session_info.id,
                    tool_name = %tool_name,
                    run_len,
                    "action stationarity: stopping turn after repeated identical tool calls"
                );
                xai_grok_telemetry::unified_log::warn(
                    "shell.turn.action_stationarity_stop",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!({
                        "loop_index": loop_index,
                        "tool_name": tool_name,
                        "run_len": run_len,
                    })),
                );
                let notice = format!(
                    "Stopped: the agent ran the same command (`{tool_name}`) {run_len} times in \
                     a row with no change in the result. If it's waiting on a long-running job, \
                     use a background task or the `monitor` tool (or a single `sleep` then check) \
                     instead of polling; otherwise send a new instruction."
                );
                self.send_update(
                    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                        acp::ContentBlock::Text(acp::TextContent::new(notice)),
                    )),
                    None,
                )
                .await;
                return Ok(TurnOutcome::Cancelled {
                    category: Some(
                        crate::session::events::CancellationCategory::ActionStationarity,
                    ),
                    context: Some(serde_json::json!({
                        "tool_name": tool_name,
                        "run_len": run_len,
                    })),
                });
            }
            self.drain_pending_interjections().await;
            self.flush_pending_skill_reminders().await;
            self.inject_pending_monitor_events().await;
            let memory_reminder = self.first_turn_memory_reminder().await;
            if memory_reminder.is_some() {
                self.memory
                    .injection_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::info!(
                    target: xai_grok_telemetry::memory_log::TARGET,
                    "MEMORY_INJECT: first-turn memory context injected"
                );
            }
            self.maybe_inject_mcp_reminder().await;
            if self.tool_context.task_output_token_budget.is_none()
                && self.two_pass_active()
                && !self.compaction.prefire.has_cache()
                && self.should_prefire_two_pass().await
                && self.compaction.prefire.try_begin()
            {
                let actor = std::sync::Arc::clone(self);
                let handle = tokio::task::spawn_local(async move {
                    actor.run_prefire_pass1().await;
                });
                self.compaction.prefire.set_handle(handle);
            }
            if self.tool_context.task_output_token_budget.is_none() {
                self.refresh_token_if_expired().await;
            }
            if self.tool_context.task_output_token_budget.is_none()
                && let Some(trigger_info) = self.check_auto_compact_needed().await
                && let Err(e) = self.run_compact_only(trigger_info).await
            {
                tracing::error!(error = %e, "Pre-sampling auto-compaction failed");
                if Self::is_auth_compact_error(&e) {
                    return Err(self.surface_compact_auth_failure(e).await);
                }
            }
            let backend_search_active = self.backend_search_active();
            tracing::debug!(
                backend_search_active,
                "backend_search: turn tool resolution"
            );
            let mut effective_tools: Vec<ToolSpec> =
                if let Some(ref override_tools) = self.forked_tool_override {
                    override_tools.clone()
                } else {
                    self.turn_base_tool_specs(&tool_definitions)
                };
            if structured_output_tool && let Some(schema) = json_schema.clone() {
                effective_tools.push(ToolSpec {
                    name: STRUCTURED_OUTPUT_TOOL.to_string(),
                    description: Some(
                        "Return your final answer as JSON matching the required schema. \
                         Call this exactly once, at the end."
                            .to_string(),
                    ),
                    parameters: schema,
                });
            }
            let build_req_start = std::time::Instant::now();
            let request = self
                .chat_state_handle
                .build_request(
                    effective_tools,
                    memory_reminder,
                    self.memory.is_enabled(),
                    trace_gcs_config
                        .clone()
                        .map(|cfg| -> Box<dyn crate::sampling::TraceContext> {
                            Box::new(crate::sampling::ConversationRequestTrace {
                                gcs_config: cfg,
                                artifact_tracker: artifact_tracker.cloned(),
                            })
                        }),
                    self.session_info.id.to_string(),
                    req_id.to_owned(),
                )
                .await
                .expect("chat state actor should be alive");
            xai_grok_telemetry::unified_log::debug(
                "shell.turn.build_request_done",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({
                    "build_request_ms": build_req_start.elapsed().as_millis() as u64,
                    "loop_index": loop_index,
                })),
            );
            let mut request = request;
            request.x_grok_session_id = Some(self.session_info.id.to_string());
            request.x_grok_turn_idx =
                Some(self.chat_state_handle.get_prompt_index().await.to_string());
            request.x_grok_agent_id = Some(xai_grok_telemetry::id::agent_id());
            if request.x_grok_deployment_id.is_none() {
                request.x_grok_deployment_id = crate::managed_config::resolve_deployment_id(
                    crate::managed_config::resolve_deployment_key().as_deref(),
                );
            }
            if structured_output_native {
                request.json_schema = json_schema.clone();
            }
            request.hosted_tools = self.hosted_tools_for_turn();
            request.max_output_tokens = self
                .tool_context
                .clamp_task_model_request(request.max_output_tokens)
                .map_err(|message| acp::Error::internal_error().data(message))?;
            self.emit_event(crate::session::events::Event::PhaseChanged {
                phase: crate::session::events::Phase::WaitingForModel,
            });
            self.observability_bridge
                .emit(
                    xai_tool_protocol::session_event::SessionEvent::PhaseChanged {
                        phase: xai_tool_protocol::session_event::SessionPhase::Sampling,
                    },
                )
                .await;
            xai_grok_telemetry::unified_log::info(
                "shell.turn.inference_start",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({
                    "loop_index": loop_index,
                    "elapsed_since_turn_start_ms": conv_turn_start.elapsed().as_millis() as u64,
                })),
            );
            let model_timer = std::time::Instant::now();
            let (response, latency) = match self.run_turn_via_sampler(request.clone()).await {
                Ok(SamplerTurnOutcome::Response(r, latency)) => (r, latency),
                Err(error) => {
                    self.tool_context.fail_task_output_usage_closed();
                    return Err(error);
                }
                Ok(SamplerTurnOutcome::CompactAndResubmit) => {
                    auth_retry_schedule.reset();
                    continue;
                }
                Ok(SamplerTurnOutcome::RefreshAuthAndResubmit) => {
                    if let Some((attempt, delay)) = auth_retry_schedule.next_delay() {
                        let delay_ms = delay.as_millis() as u64;
                        tracing::warn!(
                            attempt,
                            delay_ms,
                            "auth 401 retry: backing off before resubmit"
                        );
                        xai_grok_telemetry::unified_log::warn(
                            "shell.turn.auth_retry_backoff",
                            Some(self.session_info.id.0.as_ref()),
                            Some(serde_json::json!({
                                "loop_index": loop_index,
                                "attempt": attempt,
                                "max_retries": AuthRetrySchedule::MAX_RETRIES,
                                "delay_ms": delay_ms,
                            })),
                        );
                        self.send_xai_notification(XaiSessionUpdate::RetryState(
                            crate::extensions::notification::RetryState::Retrying {
                                attempt,
                                max_retries: AuthRetrySchedule::MAX_RETRIES,
                                reason: "Re-authenticated after 401; retrying request".to_string(),
                            },
                        ))
                        .await;
                        sleep(delay).await;
                        continue;
                    }
                    let msg = format!(
                        "Auth recovery succeeded but inference request was \
                         still rejected (401) after {} retries",
                        AuthRetrySchedule::MAX_RETRIES
                    );
                    tracing::error!(msg);
                    return Err(acp::Error::internal_error().data(
                        crate::sampling::error::error_data_with_status(msg, Some(401)),
                    ));
                }
            };
            auth_retry_schedule.reset();
            let model_elapsed_ms = model_timer.elapsed().as_millis() as u64;
            let usage = response.usage.as_ref();
            let prompt_tokens = usage.map(|u| u.prompt_tokens);
            let cached_prompt_tokens = usage.map(|u| u.cached_prompt_tokens);
            let completion_tokens = usage.map(|u| u.completion_tokens);
            let reasoning_tokens = usage.map(|u| u.reasoning_tokens);
            let ttft_ms = latency.time_to_first_token_ms;
            let tokens_per_sec = match completion_tokens {
                Some(ct) if ct > 0 => {
                    let decode_ms = match ttft_ms {
                        Some(ttft) if model_elapsed_ms > ttft => model_elapsed_ms - ttft,
                        _ => model_elapsed_ms,
                    };
                    (decode_ms > 0).then(|| {
                        let tps = f64::from(ct) * 1000.0 / decode_ms as f64;
                        (tps * 10.0).round() / 10.0
                    })
                }
                _ => None,
            };
            xai_grok_telemetry::unified_log::info(
                "shell.turn.inference_done",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({
                    "loop_index": loop_index,
                    "model_elapsed_ms": model_elapsed_ms,
                    "elapsed_since_turn_start_ms": conv_turn_start.elapsed().as_millis() as u64,
                    "ttft_ms": ttft_ms,
                    "itl_p50_ms": latency.itl_p50_ms,
                    "attempts": latency.attempts,
                    "prompt_tokens": prompt_tokens,
                    "cached_prompt_tokens": cached_prompt_tokens,
                    "completion_tokens": completion_tokens,
                    "reasoning_tokens": reasoning_tokens,
                    "tokens_per_sec": tokens_per_sec,
                })),
            );
            if let Some(usage) = response.usage.as_ref() {
                self.chat_state_handle
                    .record_token_usage(u64::from(usage.total_tokens));
                self.send_available_commands_update().await;
            }
            turn_span_totals.record(&tracing::Span::current(), &response);
            let _ = self.compaction.auto_compact_suppressed.compare_exchange(
                crate::session::compaction_config::SUPPRESS_UNTIL_SUCCESS,
                crate::session::compaction_config::SUPPRESS_NONE,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            );
            self.clear_auth_compact_suppression();
            let model_duration_ms = model_timer.elapsed().as_millis() as u64;
            {
                let model_id = self.current_model_id().await;
                xai_grok_telemetry::session_ctx::log_event(
                    xai_grok_telemetry::events::ModelResponseReceived {
                        model_id,
                        duration_ms: model_duration_ms,
                        stop_reason: response
                            .stop_reason
                            .as_ref()
                            .map(|r| format!("{r:?}").to_ascii_lowercase()),
                        prompt_tokens: response.usage.as_ref().map(|u| u.prompt_tokens),
                        completion_tokens: response.usage.as_ref().map(|u| u.completion_tokens),
                        reasoning_tokens: response.usage.as_ref().map(|u| u.reasoning_tokens),
                        cached_prompt_tokens: response
                            .usage
                            .as_ref()
                            .map(|u| u.cached_prompt_tokens),
                    },
                );
            }
            self.record_response_token_usage(&response, Some(model_duration_ms));
            if let Some(pt) = prompt_timing.take() {
                let mcp_count = self.mcp_state.lock().await.configs.len() as u32;
                let mcp_tools = self
                    .agent
                    .borrow()
                    .tool_bridge()
                    .tool_definitions()
                    .await
                    .iter()
                    .filter(|t| t.function.name.contains("__"))
                    .count() as u32;
                let turn_index = self
                    .chat_state_handle
                    .get_prompt_index()
                    .await
                    .saturating_sub(1) as u32;
                pt.emit(
                    model_duration_ms,
                    turn_index,
                    mcp_count,
                    mcp_tools,
                    self.mcp_strategy,
                    self.current_model_id().await,
                );
            }
            let mut tool_calls = response.tool_calls().to_vec();
            metrics_drop_guard.record_model_response(tool_calls.len());
            if let Some(fp) = response
                .assistant()
                .and_then(|a| a.model_fingerprint.clone())
            {
                model_fingerprint = Some(fp);
            }
            let fallback_text = response.fallback_text();
            let stop_reason = response.stop_reason;
            let response_is_empty = response.is_empty();
            let turn_refused =
                stop_reason == Some(xai_grok_sampling_types::StopReason::ContentFilter);
            let refusal_explanation = response.stop_message.clone();
            let final_answer_text = json_schema.is_some().then(|| response.assistant_text());
            for item in response.items {
                match item {
                    xai_grok_sampling_types::ConversationItem::Assistant(_) => {
                        self.record_assistant_response(item).await;
                    }
                    _ => {
                        self.chat_state_handle.push_tool_result(item);
                    }
                }
            }
            if let Some(text) = fallback_text {
                tracing::warn!(
                    text_len = text.len(),
                    "emitting fallback AgentMessageChunk — no text chunks were streamed"
                );
                self.send_update(
                    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                        acp::ContentBlock::Text(acp::TextContent::new(text)),
                    )),
                    None,
                )
                .await;
            }
            if turn_refused && response_is_empty {
                let mut notice = "The model provider refused to generate a response \
                     for this turn (content filter)."
                    .to_string();
                if let Some(explanation) = refusal_explanation.as_deref() {
                    notice.push_str("\n\nProvider explanation: ");
                    notice.push_str(explanation);
                }
                tracing::warn!(
                    has_explanation = refusal_explanation.is_some(),
                    "model response was a provider refusal — emitting notice chunk"
                );
                self.send_update(
                    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                        acp::ContentBlock::Text(acp::TextContent::new(notice)),
                    )),
                    None,
                )
                .await;
            }
            if tool_calls.is_empty() {
                if !schema_ok
                    && !turn_refused
                    && let Some(gate_cfg) = self.todo_gate_policy()
                {
                    let collected = self.collect_todo_gate_input(req_id).await;
                    let input = collected.as_input();
                    if let TodoGateDecision::Nudge { reminder, reason } = evaluate_todo_gate(&input)
                    {
                        if todo_gate_fires < gate_cfg.max_fires_per_prompt {
                            todo_gate_fires += 1;
                            tracing::info!(
                                prompt_id = %req_id,
                                pending = ?input.pending,
                                unbacked_in_progress = ?input.in_progress_unbacked,
                                backed_in_progress = ?input.in_progress_backed,
                                backing_task_count = input.backing_task_count,
                                todo_gate_fires,
                                reason = reason.as_str(),
                                "turn-end TodoGate: nudging model to advance remaining todos"
                            );
                            self.events
                                .emit(crate::session::events::Event::TodoGateFired {
                                    fires: todo_gate_fires,
                                    pending: input.pending.len(),
                                    in_progress: input.in_progress_unbacked.len()
                                        + input.in_progress_backed.len(),
                                    reason: reason.as_str(),
                                });
                            let rendered = self
                                .tool_bridge_handle()
                                .render_prompt(&reminder, &serde_json::json!({}))
                                .await
                                .unwrap_or(reminder);
                            self.push_system_reminder(&rendered);
                            continue;
                        }
                        let cap = gate_cfg.max_fires_per_prompt;
                        tracing::warn!(
                            prompt_id = %req_id,
                            todo_gate_cap = cap,
                            "turn-end TodoGate: exhausted retries, falling through"
                        );
                        self.events
                            .emit(crate::session::events::Event::TodoGateExhausted {
                                pending: input.pending.len(),
                            });
                        self.push_system_reminder(&format!(
                            "The agent attempted to end this turn {cap} times \
                             with todos still pending or in_progress. Falling through \
                             to user. If you want autonomous progress, prompt the agent \
                             to continue explicitly, or clean up the todo list."
                        ));
                    }
                }
                if self.drain_pending_interjections().await {
                    tracing::info!("Drained interjection(s) before turn completion — continuing");
                    continue;
                }
                let snapshot = self
                    .finalize_turn_bookkeeping(
                        req_id,
                        conv_turn_start,
                        &turn_span_totals,
                        model_fingerprint.clone(),
                    )
                    .await;
                if self.drain_pending_interjections().await {
                    tracing::info!(
                        "Drained late interjection(s) during turn-end bookkeeping — continuing"
                    );
                    continue;
                }
                let structured_output = match (
                    structured_output_validator.as_ref(),
                    final_answer_text.as_ref(),
                ) {
                    (Some(validator), Some(text)) => {
                        Some(validate_structured_output(validator, text))
                    }
                    _ => None,
                };
                return Ok(TurnOutcome::Completed {
                    snapshot: Box::new(snapshot),
                    tools_called: turn_tools_called,
                    structured_output,
                    refusal: turn_refused.then(|| refusal_explanation.clone().unwrap_or_default()),
                });
            }
            if structured_output_tool && let Some(validator) = structured_output_validator.as_ref()
            {
                match self
                    .handle_structured_output_tool_call(
                        &mut tool_calls,
                        validator,
                        &mut structured_output_retries,
                    )
                    .await
                {
                    StructuredOutputStep::Complete(validated) => {
                        turn_tools_called.push(STRUCTURED_OUTPUT_TOOL.to_string());
                        let snapshot = self
                            .finalize_turn_bookkeeping(
                                req_id,
                                conv_turn_start,
                                &turn_span_totals,
                                model_fingerprint.clone(),
                            )
                            .await;
                        return Ok(TurnOutcome::Completed {
                            snapshot: Box::new(snapshot),
                            tools_called: turn_tools_called,
                            structured_output: Some(validated),
                            refusal: None,
                        });
                    }
                    StructuredOutputStep::Retry => continue,
                    StructuredOutputStep::Proceed => {}
                }
            }
            for tc in &tool_calls {
                if let Some((server, tool)) =
                    crate::session::mcp_servers::parse_mcp_tool_name(&tc.name)
                {
                    let span = tracing::Span::current();
                    span.record("mcp_server.name", server.as_str());
                    span.record("mcp_tool.name", tool.as_str());
                }
                turn_tools_called.push(tc.name.clone());
            }
            let step_signature = tool_calls
                .iter()
                .map(|tc| format!("{}\u{1f}{}", tc.name, tc.arguments.as_ref()))
                .collect::<Vec<_>>()
                .join("\u{1e}");
            let step_tool_name = tool_calls
                .first()
                .map(|tc| tc.name.clone())
                .unwrap_or_default();
            let identical_run_len = identical_tool_calls.observe(&step_signature, &step_tool_name);
            if identical_run_len == NUDGE_AFTER_IDENTICAL_TOOL_CALLS {
                tracing::warn!(
                    session_id = %self.session_info.id,
                    tool_name = %step_tool_name,
                    run_len = identical_run_len,
                    "action stationarity: nudging model to break repeated identical tool calls"
                );
                xai_grok_telemetry::unified_log::warn(
                    "shell.turn.action_stationarity_nudge",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!({
                        "loop_index": loop_index,
                        "tool_name": step_tool_name,
                        "run_len": identical_run_len,
                    })),
                );
                let reminder = self
                    .tool_bridge_handle()
                    .render_prompt(
                        ACTION_STATIONARITY_NUDGE_TEMPLATE,
                        &serde_json::json!({
                            "tool_name": step_tool_name,
                            "run_len": identical_run_len,
                        }),
                    )
                    .await
                    .unwrap_or_else(|| ACTION_STATIONARITY_NUDGE_TEMPLATE.to_string());
                self.push_system_reminder(&reminder);
            }
            let tool_call_responses: Vec<ToolCallResponse> = tool_calls
                .into_iter()
                .map(|tc| ToolCallResponse {
                    id: tc.id.as_ref().to_owned(),
                    kind: "function".to_string(),
                    function: crate::sampling::types::ToolCallFunction {
                        name: tc.name,
                        arguments: tc.arguments.as_ref().to_owned(),
                    },
                })
                .collect();
            self.emit_event(crate::session::events::Event::PhaseChanged {
                phase: crate::session::events::Phase::ToolExecution,
            });
            self.observability_bridge
                .emit(
                    xai_tool_protocol::session_event::SessionEvent::PhaseChanged {
                        phase: xai_tool_protocol::session_event::SessionPhase::ToolExecution,
                    },
                )
                .await;
            let execute_tool_calls_result = self.execute_tool_calls(tool_call_responses).await;
            match execute_tool_calls_result {
                Ok(ToolLoop::PermissionReject { tool_name, reason }) => {
                    return Ok(TurnOutcome::Cancelled {
                        category: Some(
                            crate::session::events::CancellationCategory::PermissionRejected,
                        ),
                        context: Some(serde_json::json!({
                            "tool_name": tool_name,
                            "reason": reason,
                        })),
                    });
                }
                Ok(ToolLoop::HookDenied { .. }) => {}
                Ok(ToolLoop::Cancelled) => {
                    return Ok(TurnOutcome::Cancelled {
                        category: Some(
                            crate::session::events::CancellationCategory::PermissionCancelled,
                        ),
                        context: None,
                    });
                }
                Ok(ToolLoop::FollowupMessage(followup_message)) => {
                    self.add_followup_message_as_user_turn(&followup_message)
                        .await;
                    continue;
                }
                _ => {}
            }
            let next_turn = tool_turn_count + 1;
            if let Some(limit) = self.max_turns
                && next_turn > limit
            {
                tracing::info!(
                    session_id = %self.session_info.id,
                    tool_turn_count,
                    limit,
                    "max-turns limit reached, stopping"
                );
                return Ok(TurnOutcome::MaxTurnsReached { limit });
            }
            tool_turn_count = next_turn;
            if self.tool_context.task_output_token_budget.is_none()
                && let Some(trigger_info) = self.check_preflight_overflow().await
            {
                if let Err(e) = self.run_compact_only(trigger_info).await {
                    tracing::error!(error = %e, "Preflight overflow compaction failed");
                    if Self::is_auth_compact_error(&e) {
                        return Err(self.surface_compact_auth_failure(e).await);
                    }
                }
                continue;
            }
        }
    }
}
const MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS: u32 = 16;
const NUDGE_AFTER_IDENTICAL_TOOL_CALLS: u32 = 8;
const _: () = assert!(NUDGE_AFTER_IDENTICAL_TOOL_CALLS < MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS);
const ACTION_STATIONARITY_NUDGE_TEMPLATE: &str = "You have called the same tool \
     (`${{ tool_name }}`) with the exact same arguments ${{ run_len }} times in a row, \
     getting the same result each time — you appear to be stuck in a polling loop. Stop \
     repeating this call. If you are waiting on a long-running job or command, use a \
     background task${%- if tools.by_kind.monitor %} or the `${{ tools.by_kind.monitor }}` \
     tool${%- endif %}, or run a single `sleep` and then check once — do not poll in a tight \
     loop. If you cannot make progress, stop and tell the user what you are waiting for. This \
     turn will be halted automatically if the identical call keeps repeating.";
fn hash_step_signature(signature: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    signature.hash(&mut hasher);
    hasher.finish()
}
#[derive(Default)]
struct IdenticalToolCallRun {
    last_signature_hash: Option<u64>,
    tool_name: String,
    run_len: u32,
}
impl IdenticalToolCallRun {
    fn observe(&mut self, signature: &str, tool_name: &str) -> u32 {
        let hash = hash_step_signature(signature);
        if self.last_signature_hash == Some(hash) {
            self.run_len += 1;
        } else {
            self.run_len = 1;
            self.last_signature_hash = Some(hash);
        }
        self.tool_name = tool_name.to_string();
        self.run_len
    }
}
#[cfg(test)]
mod identical_tool_call_run_tests {
    use super::{IdenticalToolCallRun, MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS};
    #[test]
    fn counts_consecutive_identical_calls() {
        let mut run = IdenticalToolCallRun::default();
        let sig = "run_terminal_cmd\u{1f}{\"command\":\"squeue\"}";
        assert_eq!(run.observe(sig, "run_terminal_cmd"), 1);
        assert_eq!(run.observe(sig, "run_terminal_cmd"), 2);
        assert_eq!(run.observe(sig, "run_terminal_cmd"), 3);
    }
    #[test]
    fn a_different_call_resets_the_run() {
        let mut run = IdenticalToolCallRun::default();
        run.observe("a", "a");
        run.observe("a", "a");
        assert_eq!(run.observe("b", "b"), 1, "a different signature resets");
        assert_eq!(run.observe("b", "b"), 2);
        assert_eq!(run.tool_name, "b");
        assert_eq!(
            run.observe("a", "a"),
            1,
            "not consecutive with the first run"
        );
    }
    #[test]
    fn run_reaches_the_bound_after_n_identical_calls() {
        let mut run = IdenticalToolCallRun::default();
        let mut last = 0;
        for _ in 0..MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS {
            last = run.observe("same", "same");
        }
        assert_eq!(last, MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS);
    }
}
/// Backoff schedule for resubmits after a *successful* 401 auth recovery
/// (fresh token minted, request to be re-sent).
///
/// Two hard-won invariants, both regressions from the silent-hang incident
/// where a turn froze 16m40s and then 11.6 days (user-cancelled at 27min):
///
/// - **Delays must be 1s/2s/4s.** `tokio_retry::ExponentialBackoff::
///   from_millis(base)` raises `base` to the attempt number, so the base must
///   stay small: `from_millis(1000)` yields 1000ⁿ ms = 1s → 16m40s → 11.57
///   days. `from_millis(2).factor(500)` yields 2ⁿ × 500ms = 1s, 2s, 4s.
/// - **The schedule is per-incident, not per-turn.** A long turn can span
///   several hourly gateway token rotations; each rotation is an independent
///   401→refresh→retry event. Without `reset()` after a successful response,
///   the third rotation of one turn would land on the last (largest) delay
///   and the fourth would fail the turn outright.
struct AuthRetrySchedule {
    delays: std::iter::Take<ExponentialBackoff>,
    attempt: u32,
}
impl AuthRetrySchedule {
    /// Consecutive post-recovery 401s tolerated before the turn fails.
    const MAX_RETRIES: u32 = 3;
    fn new() -> Self {
        Self {
            delays: ExponentialBackoff::from_millis(2)
                .factor(500)
                .max_delay(std::time::Duration::from_secs(10))
                .take(Self::MAX_RETRIES as usize),
            attempt: 0,
        }
    }
    /// Next `(attempt_number, delay)` (1-indexed), or `None` once exhausted.
    fn next_delay(&mut self) -> Option<(u32, std::time::Duration)> {
        let delay = self.delays.next()?;
        self.attempt += 1;
        Some((self.attempt, delay))
    }
    /// A successful model response closes the incident: restart the schedule
    /// so the next token rotation starts back at the shortest delay.
    fn reset(&mut self) {
        *self = Self::new();
    }
}
#[cfg(test)]
mod auth_retry_schedule_tests {
    use super::AuthRetrySchedule;
    use std::time::Duration;
    /// Pins the exact schedule. Guards against the `from_millis(1000)`
    /// footgun (baseⁿ semantics): that spelling produced sleeps of 1s,
    /// 16m40s, and 11.57 days, observed in the field as a silent
    /// ~27-minute hang in `waiting_model` that the user had to cancel.
    #[test]
    fn schedule_is_one_two_four_seconds_then_exhausted() {
        let mut schedule = AuthRetrySchedule::new();
        let steps: Vec<_> = std::iter::from_fn(|| schedule.next_delay()).collect();
        assert_eq!(
            steps,
            vec![
                (1, Duration::from_secs(1)),
                (2, Duration::from_secs(2)),
                (3, Duration::from_secs(4)),
            ],
        );
        assert_eq!(
            schedule.next_delay(),
            None,
            "must exhaust after MAX_RETRIES"
        );
    }
    /// Each successful response must restart the schedule: hourly token
    /// rotations within one long turn are independent incidents, so they
    /// must not escalate toward exhaustion (turn failure).
    #[test]
    fn reset_restarts_delays_and_attempt_numbering() {
        let mut schedule = AuthRetrySchedule::new();
        schedule.next_delay();
        schedule.next_delay();
        schedule.reset();
        assert_eq!(schedule.next_delay(), Some((1, Duration::from_secs(1))));
    }
}
#[cfg(test)]
mod user_echo_broadcast_tests {
    use super::{UserEchoMode, user_echo_mode};
    /// Notification-drain: persisted (rewind/fork count user-chunk runs as
    /// turn boundaries) but never broadcast live; the pager hides it via the
    /// `hideFromScrollback` chunk meta.
    #[test]
    fn notification_drain_turn_is_persist_only() {
        assert_eq!(
            user_echo_mode("notifications-019e0000-0000-7000-8000-0000000000aa"),
            UserEchoMode::PersistOnly
        );
    }
    /// Real user prompts, cron (`/loop`) fires, and other turns still broadcast
    /// live so multi-client / dashboard viewers stay in sync.
    #[test]
    fn user_and_cron_turns_broadcast_live() {
        assert_eq!(user_echo_mode("my-prompt"), UserEchoMode::Broadcast);
        assert_eq!(
            user_echo_mode("scheduler-fired-abc"),
            UserEchoMode::Broadcast
        );
        assert_eq!(
            user_echo_mode("task-completed-bg-1"),
            UserEchoMode::Broadcast
        );
        assert_eq!(
            user_echo_mode("subagent-completed-xyz"),
            UserEchoMode::Broadcast
        );
    }
    /// Interject-fallback turns are persist-only: every pane already rendered
    /// the text from the `x.ai/session/interjection` broadcast, so a live
    /// echo would duplicate the block.
    #[test]
    fn interject_fallback_turn_is_persist_only() {
        assert_eq!(
            user_echo_mode("interject-fallback-019e24b7"),
            UserEchoMode::PersistOnly
        );
    }
}
#[cfg(test)]
mod structured_output_validation_tests {
    use super::validate_structured_output;
    fn validator() -> Result<jsonschema::Validator, String> {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"name": {"type": "string"}, "age": {"type": "integer"}},
            "required": ["name", "age"],
            "additionalProperties": false,
        });
        jsonschema::validator_for(&schema).map_err(|e| e.to_string())
    }
    #[test]
    fn accepts_conforming_json() {
        let v = validate_structured_output(&validator(), r#"{"name":"alice","age":30}"#).unwrap();
        assert_eq!(v["name"], "alice");
    }
    #[test]
    fn rejects_non_json() {
        let err = validate_structured_output(&validator(), "not json").unwrap_err();
        assert!(err.starts_with("model output was not valid JSON: "));
    }
    #[test]
    fn rejects_schema_violation() {
        let err = validate_structured_output(&validator(), r#"{"name":"alice"}"#).unwrap_err();
        assert!(err.starts_with("output does not match the required schema: "));
    }
    #[test]
    fn surfaces_invalid_schema_error() {
        let bad: Result<jsonschema::Validator, String> = Err("invalid output schema: boom".into());
        let err = validate_structured_output(&bad, r#"{"name":"alice","age":1}"#).unwrap_err();
        assert_eq!(err, "invalid output schema: boom");
    }
}
