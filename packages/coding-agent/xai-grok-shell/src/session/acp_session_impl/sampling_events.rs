//! Sampler event translation and request-lifecycle side effects.

use super::*;

impl SessionActor {
    async fn send_thought_chunk(&self, text: String, chunk_index: u64) {
        self.send_update(
            acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new(text),
            ))),
            Some(chunk_index),
        )
        .await;
    }

    /// Translate one [`xai_grok_sampler::SamplingEvent`] from the
    /// per-session sampler actor into the corresponding ACP / shell
    /// side-effects (notifications, signal recording, model-metadata
    /// refresh, etc.).
    ///
    /// Called from the drainer task spawned in `spawn_session_actor`,
    /// which loops `while let Some(event) = sampler_event_rx.recv().await`.
    /// Pure event mapping. Semantic recovery (compaction, friendly
    /// errors) lives in [`Self::handle_sampling_failure`] and runs in
    /// the turn loop, not here, because it depends on per-turn state
    /// and may need to call back into `sampler_handle.update_config`
    /// or resubmit.
    pub(crate) async fn handle_sampling_event(
        self: &Arc<Self>,
        event: xai_grok_sampler::SamplingEvent,
    ) {
        use xai_grok_sampler::{SamplingChannel, SamplingEvent};

        let request_owned = self
            .turn_stream_drained
            .lock()
            .contains_key(event.request_id());
        let owns_pending_strip = self
            .pending_image_strip
            .lock()
            .contains_key(event.request_id());
        // Map presence owns every FIFO event for the request. `None` means only
        // that the ordering waiter timed out; queued chunks remain valid until
        // the terminal event or a turn boundary removes the entry. A pending
        // strip independently retains only its persistable strip/terminal
        // lifecycle, and a late backend-tool completion may close a visible card.
        let closes_backend_tool = matches!(event, SamplingEvent::BackendToolCallCompleted { .. });
        let resolves_pending_strip = match &event {
            SamplingEvent::ImagesStripped {
                stripped_urls,
                reason,
                ..
            } => owns_pending_strip && Self::should_defer_image_strip(stripped_urls, reason),
            SamplingEvent::Completed { .. } | SamplingEvent::Failed { .. } => owns_pending_strip,
            _ => false,
        };
        if !request_owned && !closes_backend_tool && !resolves_pending_strip {
            return;
        }

        match event {
            SamplingEvent::StreamStarted {
                request_id,
                timestamp_ms,
            } => {
                // Begin a fresh per-generation segment. A NEW turn (the
                // prompt id changed) resets the whole accumulator so an
                // in-flight capture from an earlier turn cannot leak into the
                // current trace; a same-turn restart — a doomloop's next
                // reasoning-only generation under the same prompt — keeps the
                // segments already collected and just opens a new one, so
                // every generation survives instead of only the last.
                // `current_prompt_id` / `current_turn_number` are set by the
                // prompt handler before any sampler events arrive. Panic on
                // lock poison to match the file convention.
                {
                    let prompt_id = self
                        .current_prompt_id
                        .lock()
                        .expect("current_prompt_id mutex poisoned")
                        .clone();
                    let mut cap = self.streaming_turn_capture.lock();
                    if cap.prompt_id.as_deref() != prompt_id.as_deref() {
                        cap.begin_turn(prompt_id, self.current_turn_number.get());
                    }
                    cap.start_request_stream(request_id.as_str(), timestamp_ms);
                }
                self.chat_state_handle.record_stream_start(timestamp_ms);
            }
            SamplingEvent::FirstToken { .. } => {
                self.emit_event(crate::session::events::Event::FirstToken);
            }
            SamplingEvent::ChannelToken {
                request_id,
                channel,
                text,
                chunk_index,
            } => match channel {
                SamplingChannel::Text => {
                    // Out-of-band trace accumulator (never enters chat_state).
                    // See `StreamingTurnCapture` for the lifecycle.
                    {
                        let mut cap = self.streaming_turn_capture.lock();
                        if cap.prompt_id.is_none() {
                            let prompt_id = self
                                .current_prompt_id
                                .lock()
                                .expect("current_prompt_id mutex poisoned")
                                .clone();
                            cap.begin_turn(prompt_id, self.current_turn_number.get());
                            // `StreamStarted` was dropped; count this generation
                            // so `attempt_count` mirrors the real-StreamStarted
                            // path (no timestamp available, so none is stamped).
                            cap.attempt_count += 1;
                        }
                        cap.claim_current_request(request_id.as_str());
                        cap.append(false, &text);
                    }

                    // Phase change emitted alongside each text-delta
                    // event so the UI flips to "streaming text" the
                    // moment content starts arriving (the
                    // `PhaseChanged` event itself is idempotent on
                    // the consumer side).
                    self.emit_event(crate::session::events::Event::PhaseChanged {
                        phase: crate::session::events::Phase::StreamingText,
                    });
                    self.send_update(
                        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                            acp::ContentBlock::Text(acp::TextContent::new(text)),
                        )),
                        Some(chunk_index),
                    )
                    .await;
                }
                SamplingChannel::Reasoning => {
                    // Out-of-band trace accumulator (never enters chat_state).
                    {
                        let mut cap = self.streaming_turn_capture.lock();
                        if cap.prompt_id.is_none() {
                            let prompt_id = self
                                .current_prompt_id
                                .lock()
                                .expect("current_prompt_id mutex poisoned")
                                .clone();
                            cap.begin_turn(prompt_id, self.current_turn_number.get());
                            // `StreamStarted` was dropped; count this generation
                            // so `attempt_count` mirrors the real-StreamStarted
                            // path (no timestamp available, so none is stamped).
                            cap.attempt_count += 1;
                        }
                        cap.claim_current_request(request_id.as_str());
                        cap.append(true, &text);
                    }

                    self.emit_event(crate::session::events::Event::PhaseChanged {
                        phase: crate::session::events::Phase::StreamingReasoning,
                    });
                    self.send_thought_chunk(text, chunk_index).await;
                }
            },
            SamplingEvent::ToolCallDelta {
                request_id,
                tool_index,
                id,
                name,
                arguments_delta,
            } => {
                // Mark the out-of-band capture's phase so a partial taken
                // after the model started emitting a tool call records that
                // it was cut off mid tool-call rather than mid reasoning /
                // response. Only mark an already-active capture — a
                // tool-call-only turn has no reasoning/text and stays empty
                // (never uploaded), so we don't `begin_turn` here.
                {
                    let mut cap = self.streaming_turn_capture.lock();
                    if cap.prompt_id.is_some() {
                        cap.claim_current_request(request_id.as_str());
                        cap.phase = CapturePhase::ToolCall;
                    }
                }

                // Forward to clients as a `tool_call_delta_chunk` xAI session
                // update through the buffered path. Mirrors how
                // AgentMessageChunk and AgentThoughtChunk are routed -- no
                // per-chunk hook dispatch, no persistence (the canonical
                // acp::SessionUpdate::ToolCall is the source of truth for replay).
                self.send_buffered_xai_update(XaiSessionUpdate::ToolCallDeltaChunk {
                    tool_call_id: id,
                    tool_index,
                    name,
                    arguments_delta,
                })
                .await;
            }
            SamplingEvent::ResponseStarted {
                message_id,
                model,
                input_tokens,
                cache_read_input_tokens,
                cache_creation_input_tokens,
                ..
            } => {
                // Ride the buffered chunk rail so this lands ahead of the
                // response's first agent chunk (same FIFO `event_tx` as
                // `send_update`), letting headless partial framing emit the real
                // `message_start` id/input-usage in order.
                self.send_buffered_xai_update(XaiSessionUpdate::ResponseStarted {
                    message_id: Some(message_id),
                    model: Some(model),
                    input_tokens,
                    cache_read_input_tokens,
                    cache_creation_input_tokens,
                })
                .await;
            }
            SamplingEvent::ReasoningCompleted { signature, .. } => {
                // Ride the buffered chunk rail so this lands right after the
                // response's thought chunks (and before its text), letting
                // headless partial framing emit `signature_delta` before the
                // thinking block's `content_block_stop`, in order.
                self.send_buffered_xai_update(XaiSessionUpdate::ReasoningCompleted {
                    signature: Some(signature),
                })
                .await;
            }
            SamplingEvent::DoomLoopSignals {
                request_id,
                triggers,
            } => {
                let matches_active_turn = self.turn_stream_drained.lock().contains_key(&request_id);
                if matches_active_turn {
                    self.doom_loop_turn_tally
                        .lock()
                        .merge_all_triggers(&triggers);
                }
            }
            SamplingEvent::Completed {
                request_id,
                response,
                metrics: _,
            } => {
                let request_updates_turn = request_owned;

                // Retry succeeded: detached persistence acquires rewrite
                // ownership before it claims URLs. Rewind either clears queued
                // work first, or waits until the durable strip finishes.
                if self.pending_image_strip.lock().contains_key(&request_id) {
                    let session = Arc::clone(self);
                    let rid = request_id.clone();
                    tokio::task::spawn_local(async move {
                        session.apply_pending_image_strip(&rid).await;
                    });
                }
                // The awaited result is authoritative for incidence. This
                // event-side merge preserves direct-event tests and stays
                // request-bound so a late event cannot enter the next turn.
                if request_updates_turn {
                    let all_triggers: Vec<String> = response
                        .doom_loop_signals
                        .iter()
                        .map(|signal| signal.raw.clone())
                        .collect();
                    self.doom_loop_turn_tally
                        .lock()
                        .merge_all_triggers(&all_triggers);
                }

                // Telemetry: a completed response still carrying confident
                // doom-loop signals after this turn resampled is a
                // budget-spent accept. Without a prior resample (attempts 0
                // — the observe-only `max_retries = 0` policy) nothing was
                // discarded, so the signals stay warn-only: no counter, no
                // analytics event, no capture stamp.
                if request_updates_turn && let Some(policy) = self.doom_loop_recovery {
                    let triggers = policy.confident_triggers(&response.doom_loop_signals);
                    if !triggers.is_empty() {
                        let (attempts, should_count, should_stamp) = {
                            let mut tally = self.doom_loop_turn_tally.lock();
                            let attempts = tally.recovery_attempt_count(request_id.as_str());
                            if attempts == 0 {
                                (0, false, false)
                            } else {
                                let count = tally.mark_accepted_request(request_id.as_str());
                                if count {
                                    tally.merge_recovery_triggers(&triggers);
                                }
                                let stamp =
                                    tally.mark_accepted_request_stamped(request_id.as_str());
                                (attempts, count, stamp)
                            }
                        };
                        if should_stamp {
                            // Stamped BEFORE `clear_current_segment` below,
                            // which folds a text-free stamped segment for the trace.
                            self.streaming_turn_capture.lock().stamp_request_doom_loop(
                                request_id.as_str(),
                                crate::session::streaming_capture::DoomLoopSegmentStamp {
                                    doom_loop_triggers: triggers.clone(),
                                    attempt: attempts.saturating_add(1),
                                    aborted_at_chunk: None,
                                    action: "accepted_after_budget".to_string(),
                                },
                            );
                        }
                        if should_count {
                            self.signals_handle()
                                .record_doom_loop_accepted_after_budget(triggers);
                        }
                    }
                }

                // Canonical assistant response is being committed via
                // `record_assistant_response` in `process_conversation_turn`.
                // Discard the in-progress generation rather than wiping the
                // whole capture: a same-turn doomloop generation must not erase
                // earlier uncommitted ones, and this committed generation is
                // already in afterStateHistory so its reasoning must not enter
                // `segments` (nor count against the byte cap of later ones).
                // Strip-only terminal admission has no stream ownership and
                // must not alter partial capture retained for turn reporting.
                if request_updates_turn {
                    self.streaming_turn_capture
                        .lock()
                        .clear_request_segment(request_id.as_str());
                }

                // Successful-request timing and inference metrics are recorded
                // from the awaited result. This ordered rail owns only capture
                // mutation and terminal barrier release.
                // Release only after the terminal event is fully processed.
                // FIFO ordering then guarantees all preceding chunks and
                // detector signals are visible before turn teardown proceeds.
                let sender = self
                    .turn_stream_drained
                    .lock()
                    .remove(&request_id)
                    .flatten();
                if let Some(tx) = sender {
                    let _ = tx.send(());
                }
            }
            SamplingEvent::ModelMetadata { metadata, .. } => {
                self.handle_model_metadata_update(metadata).await;
            }
            SamplingEvent::ImagesStripped {
                request_id,
                stripped_urls,
                reason,
            } => {
                // Policy lives in `acp_session_impl/image_strip.rs`.
                self.handle_images_stripped(request_id, stripped_urls, reason)
                    .await;
            }
            SamplingEvent::Retrying {
                request_id,
                attempt,
                max_retries,
                kind,
                reason,
                doom_loop_triggers,
                doom_loop_aborted_at_chunk,
            } => {
                if !self.turn_stream_drained.lock().contains_key(&request_id) {
                    return;
                }
                if kind == xai_grok_sampler::SamplingErrorKind::DoomLoopDetected {
                    let triggers = doom_loop_triggers.unwrap_or_default();
                    let (should_count, should_stamp) = {
                        let mut tally = self.doom_loop_turn_tally.lock();
                        let count =
                            tally.record_recovery_attempt(request_id.as_str(), attempt, &triggers);
                        let stamp =
                            tally.mark_recovery_attempt_stamped(request_id.as_str(), attempt);
                        (count, stamp)
                    };
                    if should_stamp {
                        // Ordered event rail owns capture segment attribution.
                        self.streaming_turn_capture.lock().stamp_request_doom_loop(
                            request_id.as_str(),
                            crate::session::streaming_capture::DoomLoopSegmentStamp {
                                doom_loop_triggers: triggers.clone(),
                                attempt,
                                aborted_at_chunk: doom_loop_aborted_at_chunk,
                                action: "resampled".to_string(),
                            },
                        );
                    }
                    if should_count {
                        self.signals_handle().record_doom_loop_recovery_attempt(
                            triggers,
                            doom_loop_aborted_at_chunk,
                        );
                    }
                }
                xai_grok_telemetry::unified_log::warn(
                    "shell.turn.inference_retry",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!({
                        "sampler_request_id": request_id.as_str(),
                        "attempt": attempt,
                        "max_retries": max_retries,
                        "kind": kind.as_str(),
                        "reason": crate::util::truncate(&reason, 300),
                    })),
                );
                self.send_xai_notification(XaiSessionUpdate::RetryState(
                    crate::extensions::notification::RetryState::Retrying {
                        attempt,
                        max_retries,
                        reason,
                    },
                ))
                .await;
            }
            SamplingEvent::Failed { request_id, error } => {
                // The stripped retry (if any) did not rescue the turn:
                // nothing durable may come of it. A timeout-owned failure may
                // drop only this request's strip and record its terminal metric;
                // fully unowned failures remain stale.
                self.drop_pending_image_strip(&request_id);
                if !request_owned {
                    self.turn_stream_drained.lock().remove(&request_id);
                    return;
                }
                // Telemetry-only here. The terminal error fires through
                // `submit_and_collect`'s Result branch; the turn loop's
                // `handle_sampling_failure` decides whether to compact
                // or surface a friendly message.
                xai_grok_telemetry::unified_log::error(
                    "shell.turn.inference_failed",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!({
                        "sampler_request_id": request_id.as_str(),
                        "kind": error.kind.as_str(),
                        "status_code": error.status_code,
                        "is_retryable": error.is_retryable,
                        "message": crate::util::truncate(&error.message, 300),
                    })),
                );
                self.signals_handle()
                    .record_error_typed(error.kind.as_str());
                if let Some(ref ctx) = error.empty_response_context {
                    tracing::info!(
                        empty_response = true,
                        empty_reason = ctx.reason.as_str(),
                        had_reasoning = ctx.had_reasoning,
                        finish_reason = ctx.finish_reason_str(),
                        model = %ctx.model,
                        "sampler reported empty response (will retry if retryable)",
                    );
                }
                // Terminal failures must flush the same FIFO barrier as
                // completions. This keeps preceding detector labels in the
                // current turn even when no response is accepted.
                let sender = self
                    .turn_stream_drained
                    .lock()
                    .remove(&request_id)
                    .flatten();
                if let Some(tx) = sender {
                    let _ = tx.send(());
                }
            }
            // ── Backend-hosted tool progress ─────────────────────
            // These tools are executed server-side by the agentic
            // sampler. We emit ACP ToolCall/ToolCallUpdate so the
            // pager can show progress (e.g., "Searching the web…").
            SamplingEvent::BackendToolCallStarted { call_id, name, .. } => {
                self.signals_handle().record_tool_call(&name);
                let (title, kind, raw_input) = backend_tool_display(&name);
                self.send_update(
                    acp::SessionUpdate::ToolCall(
                        acp::ToolCall::new(
                            acp::ToolCallId::new(Arc::from(call_id.as_str())),
                            title,
                        )
                        .kind(kind)
                        .status(acp::ToolCallStatus::InProgress)
                        .content(vec![])
                        .locations(vec![])
                        .raw_input(Some(raw_input))
                        .meta(serde_json::json!({"backend": true}).as_object().cloned()),
                    ),
                    None,
                )
                .await;
            }
            SamplingEvent::BackendToolCallCompleted {
                call_id,
                name,
                result,
                ..
            } => {
                // Propagate the backend call's real success/failure: the payload's
                // `status` decides the ACP terminal status, so a backend-reported
                // failure surfaces as `Failed` (reaching the headless
                // `web_search_tool_result_error` branch) instead of a blanket
                // `Completed`.
                let status = backend_tool_call_status(result.as_ref());
                if request_owned {
                    if status == acp::ToolCallStatus::Failed {
                        self.signals_handle().record_tool_failure(&name);
                    } else {
                        self.signals_handle().record_tool_success(&name);
                    }
                }
                let (title, _kind, _raw_input) = backend_tool_display(&name);
                self.send_update(
                    acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                        acp::ToolCallId::new(Arc::from(call_id.as_str())),
                        acp::ToolCallUpdateFields::new()
                            .status(Some(status))
                            .title(Some(title))
                            .raw_output(result),
                    )),
                    None,
                )
                .await;
            }
        }
    }
}

#[cfg(test)]
#[path = "sampling_events_tests.rs"]
mod tests;
