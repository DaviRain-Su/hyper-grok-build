use super::*;

pub(super) fn turn_result_to_hook_outcome(
    result: &Result<TurnOutcome, acp::Error>,
) -> xai_tool_protocol::turn_hook::TurnHookOutcome {
    use xai_tool_protocol::turn_hook::TurnHookOutcome;
    match result {
        Ok(TurnOutcome::Completed { .. }) | Ok(TurnOutcome::StationarityEnded { .. }) => {
            TurnHookOutcome::Completed
        }
        Ok(TurnOutcome::Cancelled { .. }) | Ok(TurnOutcome::MaxTurnsReached { .. }) => {
            TurnHookOutcome::Cancelled
        }
        Err(_) => TurnHookOutcome::Error,
    }
}

/// Encode a [`CancellationCategory`](crate::session::events::CancellationCategory) as its bare snake_case wire string for the `after_turn` payload.
/// Deliberately `serde_json::to_value` then `as_str`, not `to_string`: that yields the quoted form and fails the workspace decode.
pub(super) fn cancellation_category_to_wire_string(
    category: Option<crate::session::events::CancellationCategory>,
) -> Option<String> {
    let category = category?;
    serde_json::to_value(category)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
}

/// The shell's granular `ToolOutcome` variants collapse to the hub protocol's three.
/// `Cancelled` means the tool never ran (permission, doom-loop, hook, followup).
pub(super) fn map_tool_outcome(
    outcome: crate::session::events::ToolOutcome,
) -> xai_tool_protocol::session_event::ToolCallOutcome {
    use crate::session::events::ToolOutcome;
    use xai_tool_protocol::session_event::ToolCallOutcome;
    match outcome {
        ToolOutcome::Success => ToolCallOutcome::Success,
        ToolOutcome::Error | ToolOutcome::InvalidTool => ToolCallOutcome::Error,
        ToolOutcome::PermissionRejected
        | ToolOutcome::PermissionCancelled
        | ToolOutcome::Followup
        | ToolOutcome::HookDenied
        | ToolOutcome::Cancelled => ToolCallOutcome::Cancelled,
    }
}

/// Returns `(notification_type, message, title, level)` when this update should trigger a vendor-compatible `Notification` hook.
///
/// Internal and high-frequency updates (hook scrollback, retry progress, config changes) are excluded.
/// Migrated hooks fire only on updates that need the user's attention.
/// `DiffReview` always waits on the user, so it is safe to fire `permission_prompt` here.
#[allow(clippy::type_complexity)]
pub(super) fn notification_hook_for_update(
    update: &XaiSessionUpdate,
) -> Option<(String, Option<String>, Option<String>, Option<String>)> {
    match update {
        XaiSessionUpdate::DiffReview { .. } => Some((
            "permission_prompt".into(),
            Some("Diff review requested".into()),
            None,
            Some("info".into()),
        )),
        XaiSessionUpdate::AutoRecoveryExhausted { error, .. } => Some((
            "agent_error".into(),
            Some(error.clone()),
            None,
            Some("error".into()),
        )),
        XaiSessionUpdate::RetryState(RetryState::Exhausted { reason, .. }) => Some((
            "agent_error".into(),
            Some(reason.clone()),
            None,
            Some("error".into()),
        )),
        XaiSessionUpdate::RetryState(RetryState::Failed { message, .. }) => Some((
            "agent_error".into(),
            Some(message.clone()),
            None,
            Some("error".into()),
        )),
        _ => None,
    }
}

pub(super) struct DeferredPostToolUseScrollback {
    tool_name: String,
    results: Vec<xai_grok_hooks::result::HookRunResult>,
}

impl SessionActor {
    pub(super) fn hook_run_ctx(&self) -> xai_grok_hooks::runner::RunContext<'_> {
        xai_grok_hooks::runner::RunContext {
            session_id: &self.session_info.id.0,
            workspace_root: &self.hook_resolved_workspace_root,
            process_scope: self.tool_context.process_scope.clone(),
        }
    }

    /// The annotation renders inline with the preceding tool call block rather than as a separate agent message.
    pub(super) async fn send_hook_annotation(&self, message: &str) {
        self.send_xai_notification(XaiSessionUpdate::HookAnnotation {
            message: message.to_string(),
        })
        .await;
    }

    /// `prompt_id` is `None` for session-level dispatches (session_start / session-end stop).
    pub(super) async fn send_hook_execution(
        &self,
        event_name: &str,
        tool_name: Option<&str>,
        prompt_id: Option<&str>,
        results: &[xai_grok_hooks::result::HookRunResult],
    ) {
        if results.is_empty()
            || results
                .iter()
                .all(|r| matches!(r, xai_grok_hooks::result::HookRunResult::Skipped { .. }))
        {
            return;
        }
        use crate::extensions::notification::{HookRunEntryDto, HookRunStatusDto};
        use xai_grok_hooks::result::HookRunResult;

        let runs: Vec<HookRunEntryDto> = results
            .iter()
            .map(|r| {
                let (name, status) = match r {
                    HookRunResult::Success {
                        hook_name, elapsed, ..
                    } => (
                        hook_name.clone(),
                        HookRunStatusDto::Success {
                            elapsed_ms: elapsed.as_millis() as u64,
                        },
                    ),
                    HookRunResult::Skipped { hook_name } => {
                        (hook_name.clone(), HookRunStatusDto::Skipped)
                    }
                    HookRunResult::Blocked {
                        hook_name,
                        detail,
                        elapsed,
                        ..
                    } => (
                        hook_name.clone(),
                        HookRunStatusDto::Failed {
                            error: detail.clone(),
                            elapsed_ms: elapsed.as_millis() as u64,
                            blocked: true,
                        },
                    ),
                    HookRunResult::Failed {
                        hook_name,
                        error,
                        elapsed,
                        ..
                    } => (
                        hook_name.clone(),
                        HookRunStatusDto::Failed {
                            error: error.clone(),
                            elapsed_ms: elapsed.as_millis() as u64,
                            blocked: false,
                        },
                    ),
                };
                HookRunEntryDto {
                    name,
                    status,
                    output: None,
                }
            })
            .collect();

        self.send_xai_notification(XaiSessionUpdate::HookExecution {
            event_name: event_name.to_string(),
            tool_name: tool_name.map(|s| s.to_string()),
            prompt_id: prompt_id.map(|s| s.to_string()),
            runs,
        })
        .await;

        for r in results {
            let system_message = match r {
                HookRunResult::Success { system_message, .. }
                | HookRunResult::Blocked { system_message, .. }
                | HookRunResult::Failed { system_message, .. } => system_message.as_deref(),
                HookRunResult::Skipped { .. } => None,
            };
            if let Some(message) = system_message.map(str::trim).filter(|m| !m.is_empty()) {
                self.send_hook_annotation(message).await;
            }
        }
    }

    pub(super) fn hook_workspace_root(&self) -> String {
        self.hook_resolved_workspace_root.clone()
    }

    /// Subagent type for tool-hook attribution, or `None` for the top-level session.
    /// Prefers the task `subagent_type`, falling back to the agent definition name for older spawns.
    pub(super) fn subagent_type_label(&self) -> Option<String> {
        if !self.startup_hints.is_subagent {
            return None;
        }
        Some(
            self.startup_hints
                .subagent_type
                .clone()
                .unwrap_or_else(|| self.agent.borrow().definition().name.clone()),
        )
    }

    pub(super) fn permission_mode_label(&self) -> &'static str {
        if self.plan_mode.lock().is_active() {
            "plan"
        } else if self.permissions.is_yolo_mode() {
            "bypassPermissions"
        } else if self.permissions.is_auto_mode() {
            "auto"
        } else {
            "default"
        }
    }

    /// Dispatch a non-blocking hook event: build the envelope, fire observe-only client hooks, then run the on-disk registry.
    pub(super) async fn dispatch_hook(
        &self,
        event: xai_grok_hooks::event::HookEventName,
        payload: xai_grok_hooks::event::HookPayload,
        prompt_id: Option<&str>,
        tool_name: Option<&str>,
    ) {
        if !self.may_have_hooks_for(event) {
            return;
        }
        // Fires observe-only client hooks before (and independent of) the on-disk registry guard below.
        let envelope = self.fire_hook(event, prompt_id.map(|s| s.to_string()), payload);
        let Some(registry) = self.hook_registry.borrow().clone() else {
            return;
        };
        let ctx = self.hook_run_ctx();
        // Prompt-gate events go through dispatch_prompt_submit_hook; dispatch_non_blocking debug-asserts observe-only
        let results =
            xai_grok_hooks::dispatcher::dispatch_non_blocking(&registry, event, &envelope, &ctx)
                .await;
        self.send_hook_execution(&event.to_string(), tool_name, prompt_id, &results)
            .await;
        self.emit_hook_executed_telemetry(&event.to_string(), tool_name, &results)
            .await;
    }

    pub(super) async fn dispatch_post_tool_use_hook(
        &self,
        prepared: &PreparedToolCall,
        output: &ToolsToolOutput,
        duration_ms: Option<u64>,
    ) -> (PostToolUseDelivery, Option<DeferredPostToolUseScrollback>) {
        use xai_grok_hooks::event::{HookEventName, HookPayload, truncate_payload};

        if !self.may_have_hooks_for(HookEventName::PostToolUse) {
            return (PostToolUseDelivery::default(), None);
        }

        let tool_result = serde_json::to_value(output).unwrap_or(serde_json::Value::Null);
        let raw_input: serde_json::Value =
            serde_json::from_str(&prepared.raw_arguments).unwrap_or(serde_json::Value::Null);
        let (tool_input_value, tool_input_truncated) = truncate_payload(raw_input);
        let (tool_result_value, tool_result_truncated) = truncate_payload(tool_result);
        let hook_tool_name = prepared.hook_tool_name().to_owned();

        let event = HookEventName::PostToolUse.to_string();
        let envelope = self.make_hook_envelope(
            HookEventName::PostToolUse,
            None,
            HookPayload::PostToolUse {
                tool_name: hook_tool_name.clone(),
                tool_use_id: prepared.call_id.clone(),
                tool_input: tool_input_value,
                tool_result: tool_result_value,
                tool_input_truncated,
                tool_result_truncated,
                duration_ms,
                is_backgrounded: false,
                subagent_type: self.subagent_type_label(),
            },
        );
        let registry = self.hook_registry.borrow().clone();
        let mut dispatch_result = if let Some(registry) = registry {
            let ctx = self.hook_run_ctx();
            xai_grok_hooks::dispatcher::dispatch_post_tool_use(&registry, &envelope, &ctx).await
        } else {
            xai_grok_hooks::dispatcher::PostToolUseResult::default()
        };
        // Client PostToolUse runs the awaited gate though it is not yet in
        // ADVERTISED_BLOCKING_EVENTS; SDK clients dispatch by callback id and answer.
        dispatch_result.merge(self.run_post_tool_use_client_hooks(&envelope).await);

        let mut results = std::mem::take(&mut dispatch_result.results);
        let delivery = plan_post_tool_use_delivery(
            dispatch_result,
            output,
            self.reminder_wrapper_tag(),
            &mut results,
        );

        self.emit_hook_executed_telemetry(&event, Some(&hook_tool_name), &results)
            .await;
        let deferred = DeferredPostToolUseScrollback {
            tool_name: hook_tool_name,
            results,
        };
        (delivery, Some(deferred))
    }

    pub(super) async fn emit_post_tool_use_scrollback(
        &self,
        deferred: DeferredPostToolUseScrollback,
    ) {
        let event = xai_grok_hooks::event::HookEventName::PostToolUse.to_string();
        self.send_hook_execution(&event, Some(&deferred.tool_name), None, &deferred.results)
            .await;
    }

    /// Build the `PostToolUseFailure` payload from a dispatched call and run the
    /// context-only failure path. Sole hook-presence gate for both failure
    /// sites (MCP error result and hard dispatch error).
    pub(super) async fn dispatch_tool_failure(
        &self,
        prepared: &PreparedToolCall,
        error_text: String,
        duration_ms: u64,
    ) -> Vec<xai_grok_hooks::dispatcher::AdditionalContext> {
        if !self.may_have_hooks_for(xai_grok_hooks::event::HookEventName::PostToolUseFailure) {
            return Vec::new();
        }
        let raw_input: serde_json::Value =
            serde_json::from_str(&prepared.raw_arguments).unwrap_or(serde_json::Value::Null);
        let (tool_input, tool_input_truncated) = xai_grok_hooks::event::truncate_payload(raw_input);
        let hook_tool_name = prepared.hook_tool_name();
        self.dispatch_post_tool_use_failure_hook(
            xai_grok_hooks::event::HookPayload::PostToolUseFailure {
                tool_name: hook_tool_name.to_owned(),
                tool_use_id: prepared.call_id.clone(),
                tool_input,
                tool_input_truncated,
                error: error_text,
                duration_ms: Some(duration_ms),
                // No clean abort/cancel signal at these dispatch sites; failures
                // here are tool-reported, never interrupts.
                is_interrupt: false,
                subagent_type: self.subagent_type_label(),
            },
            hook_tool_name,
        )
        .await
    }

    /// Dispatch a `PostToolUseFailure` event: fire observe-only client hooks,
    /// then run the on-disk registry's context-only failure path. Returns the
    /// aggregated `additionalContext` notes for the caller to deliver after the
    /// failed tool result. Context-only — no block or output replacement.
    async fn dispatch_post_tool_use_failure_hook(
        &self,
        payload: xai_grok_hooks::event::HookPayload,
        tool_name: &str,
    ) -> Vec<xai_grok_hooks::dispatcher::AdditionalContext> {
        let event = xai_grok_hooks::event::HookEventName::PostToolUseFailure;
        let envelope = self.fire_hook(event, None, payload);
        let Some(registry) = self.hook_registry.borrow().clone() else {
            return Vec::new();
        };
        let ctx = self.hook_run_ctx();
        let result =
            xai_grok_hooks::dispatcher::dispatch_post_tool_use_failure(&registry, &envelope, &ctx)
                .await;
        self.send_hook_execution(&event.to_string(), Some(tool_name), None, &result.results)
            .await;
        self.emit_hook_executed_telemetry(&event.to_string(), Some(tool_name), &result.results)
            .await;
        result.additional_context
    }

    /// Enforcement scope for a prompt-gate block: only a real user prompt on a top-level session.
    /// The event fires for every origin, but synthetic wakes and subagent sessions run the gate observe-only.
    pub(super) fn should_enforce_prompt_block(
        &self,
        policy: &xai_agent_lifecycle::InputPolicy,
    ) -> bool {
        policy.authority.is_human_intent() && !self.startup_hints.is_subagent
    }

    /// Run the `UserPromptSubmit` prompt gate: observe client hooks, then the on-disk registry, with shared scrollback and telemetry side effects.
    /// Returns the gate verdict; the caller decides whether to enforce it (`Block` rejects the prompt).
    pub(super) async fn dispatch_prompt_submit_hook(
        &self,
        payload: xai_grok_hooks::event::HookPayload,
        prompt_id: Option<&str>,
    ) -> xai_grok_hooks::result::PromptDecision {
        let event = xai_grok_hooks::event::HookEventName::UserPromptSubmit;
        if !self.may_have_hooks_for(event) {
            return xai_grok_hooks::result::PromptDecision::Allow;
        }
        // Fires observe-only client hooks before (and independent of) the on-disk registry guard below.
        let envelope = self.fire_hook(event, prompt_id.map(|s| s.to_string()), payload);
        let Some(registry) = self.hook_registry.borrow().clone() else {
            return xai_grok_hooks::result::PromptDecision::Allow;
        };
        let ctx = self.hook_run_ctx();
        let gate =
            xai_grok_hooks::dispatcher::dispatch_prompt_gate(&registry, &envelope, &ctx).await;
        self.send_hook_execution(&event.to_string(), None, prompt_id, &gate.results)
            .await;
        self.emit_hook_executed_telemetry(&event.to_string(), None, &gate.results)
            .await;
        gate.decision
    }

    pub(super) async fn emit_hook_executed_telemetry(
        &self,
        event_name: &str,
        tool_name: Option<&str>,
        results: &[xai_grok_hooks::result::HookRunResult],
    ) {
        let tool = tool_name.map(|s| s.to_string());
        for r in results {
            let (hook_name, elapsed, outcome) = match r {
                xai_grok_hooks::result::HookRunResult::Success {
                    hook_name, elapsed, ..
                } => (
                    hook_name,
                    elapsed,
                    xai_grok_telemetry::events::HookOutcome::Success,
                ),
                xai_grok_hooks::result::HookRunResult::Blocked {
                    hook_name, elapsed, ..
                } => (
                    hook_name,
                    elapsed,
                    xai_grok_telemetry::events::HookOutcome::Blocked,
                ),
                xai_grok_hooks::result::HookRunResult::Failed {
                    hook_name, elapsed, ..
                } => (
                    hook_name,
                    elapsed,
                    xai_grok_telemetry::events::HookOutcome::Error,
                ),
                xai_grok_hooks::result::HookRunResult::Skipped { .. } => continue,
            };
            xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::HookExecuted {
                hook_name: hook_name.clone(),
                event: event_name.to_string(),
                tool_name: tool.clone(),
                duration_ms: elapsed.as_millis() as u64,
                outcome,
            });
        }
    }
}

#[cfg(test)]
mod notification_hook_filter_tests {
    use super::*;
    use crate::extensions::notification::{
        FeedbackRequestNotification, HookRunEntryDto, HookRunStatusDto, RetryState,
    };

    #[test]
    fn hook_updates_do_not_fire_notification_hook() {
        let execution = XaiSessionUpdate::HookExecution {
            event_name: "pre_tool_use".into(),
            tool_name: Some("read_file".into()),
            prompt_id: None,
            runs: vec![HookRunEntryDto {
                name: "test".into(),
                status: HookRunStatusDto::Success { elapsed_ms: 1 },
                output: None,
            }],
        };
        assert!(notification_hook_for_update(&execution).is_none());

        let annotation = XaiSessionUpdate::HookAnnotation {
            message: "running hooks".into(),
        };
        assert!(notification_hook_for_update(&annotation).is_none());
    }

    #[test]
    fn retry_in_progress_does_not_fire_notification_hook() {
        let update = XaiSessionUpdate::RetryState(RetryState::Retrying {
            attempt: 1,
            max_retries: 3,
            reason: "timeout".into(),
            error_type: None,
        });
        assert!(notification_hook_for_update(&update).is_none());
    }

    #[test]
    fn feedback_request_does_not_fire_notification_hook() {
        let update = XaiSessionUpdate::FeedbackRequest(FeedbackRequestNotification {
            request_id: "req-1".into(),
            tier: "tier1".into(),
            prompt: "How was this session?".into(),
            dismissible: true,
            trigger_type: "tier1_engagement".into(),
            trigger_condition: "turns >= 10".into(),
            trigger_reason: "long session".into(),
            stars: true,
            thumbs: false,
            text: false,
        });
        assert!(notification_hook_for_update(&update).is_none());
    }

    #[test]
    fn diff_review_fires_permission_prompt() {
        let update = XaiSessionUpdate::DiffReview { content: vec![] };
        let (ty, message, _, level) = notification_hook_for_update(&update).expect("should fire");
        assert_eq!(ty, "permission_prompt");
        assert_eq!(message.as_deref(), Some("Diff review requested"));
        assert_eq!(level.as_deref(), Some("info"));
    }

    #[test]
    fn task_completed_does_not_fire_via_filter() {
        let update = XaiSessionUpdate::TaskCompleted {
            task_snapshot: xai_grok_tools::types::TaskSnapshot {
                task_id: "task-1".into(),
                command: "echo hi".into(),
                display_command: None,
                cwd: "/tmp".into(),
                start_time: std::time::SystemTime::UNIX_EPOCH,
                end_time: None,
                output: String::new(),
                output_file: std::path::PathBuf::from("/tmp/out"),
                truncated: false,
                exit_code: Some(0),
                signal: None,
                completed: true,
                kind: Default::default(),
                block_waited: false,
                explicitly_killed: false,
                kill_result_delivered: false,
                owner_session_id: None,
                description: None,
                is_backgrounded: false,
                output_total_bytes: 0,
            },
            will_wake: false,
        };
        assert!(notification_hook_for_update(&update).is_none());
    }
}
