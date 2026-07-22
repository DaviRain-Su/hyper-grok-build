use super::*;
use xai_grok_shell::sampling::error::format_rate_limited_user_message;
/// Stash a live stop/stop_failure batch under `stash_pid` for the turn marker
/// to fold. `merge_same_name` merges a same-name repeat instead of standalone.
pub(super) fn stash_live_stop_batch(
    agent: &mut AgentView,
    stash_pid: Option<String>,
    event_name: String,
    hook_entries: Vec<crate::scrollback::blocks::tool::HookRunEntry>,
    merge_same_name: bool,
) {
    if let Some(stale) = agent
        .pending_stop_hooks
        .take_if(|p| p.prompt_id != stash_pid)
    {
        for (name, runs) in stale.groups {
            agent.scrollback.push_lifecycle_hooks(name, runs);
        }
    }
    let pending = agent.pending_stop_hooks.get_or_insert_with(|| {
        super::super::agent_view::PendingStopHooks {
            prompt_id: stash_pid,
            groups: Vec::new(),
        }
    });
    match pending
        .groups
        .iter()
        .position(|(name, _)| *name == event_name)
    {
        Some(idx) if merge_same_name => {
            pending.groups[idx].1.extend(hook_entries);
        }
        Some(_) => {
            agent
                .scrollback
                .push_lifecycle_hooks(event_name, hook_entries);
        }
        None => {
            pending.groups.push((event_name, hook_entries));
        }
    }
}
pub(super) fn refresh_context_used(view: &mut AgentView, used: u64) {
    let total = view.session.models.get_context_window().unwrap_or(0);
    view.apply_context_used(used, total);
}
/// Refresh the bar and record `used` as the confirmed count for a pending
/// compaction message; call only from the `meta.totalTokens` path.
pub(super) fn confirm_context_used(view: &mut AgentView, used: u64) {
    refresh_context_used(view, used);
    view.session.note_context_used(used);
}
/// Replay gate shared by the ACP and xAI session-update paths. Returns `true`
/// when the update must be dropped.
///
/// Replay is only expected while a `session/load` is in flight for this agent
/// (fresh-view load or reconnect reload window). Anything else is misrouted —
/// e.g. a leader falling through to broadcast another client's replay, or a
/// replay landing after its reload already timed out — and applying it would
/// append duplicated history below the live transcript. An expected replay is
/// recorded on the open reload window instead (see
/// [`AgentView::mark_reload_replay_seen`]). One `warn!` per incident; the rest
/// of the burst (one line per replayed event) logs at `debug!`.
pub(super) fn drop_unexpected_replay(
    agent: &mut AgentView,
    meta: &NotificationMeta,
    session_id: &str,
    source: &'static str,
) -> bool {
    if !meta.is_replay {
        return false;
    }
    if agent.session.loading_replay {
        agent.mark_reload_replay_seen();
        return false;
    }
    if agent.unexpected_replay_drops == 0 {
        tracing::warn!(
            session_id,
            source,
            event_id = meta.event_id.as_deref(),
            "Dropping unexpected replay update (no session load in flight); further drops logged at debug"
        );
    } else {
        tracing::debug!(
            session_id,
            source,
            event_id = meta.event_id.as_deref(),
            "Dropping unexpected replay update"
        );
    }
    agent.unexpected_replay_drops = agent.unexpected_replay_drops.saturating_add(1);
    true
}
/// Advance the reconnect cursor to an APPLIED update's eventId. Called from
/// every applied arm (Plan, bg-stdout, tracker) — dropped updates (dedup,
/// promptId gate, unexpected replay) deliberately don't move it.
pub(super) fn advance_reconnect_cursor(agent: &mut AgentView, meta: &mut NotificationMeta) {
    if let Some(id) = meta.event_id.take() {
        agent.last_seen_event_id = Some(id);
    }
}
/// Handle `x.ai/session_notification` and replay-path `x.ai/session/update`.
///
/// Routes by `session_id` so events for an inactive agent still mutate that
/// agent's state. The redraw decision is gated on whether the matched agent
/// is the currently visible one.
pub(super) fn handle_session_notification(notif: &acp::ExtNotification, app: &mut AppView) -> bool {
    let Ok(session_notif) = serde_json::from_str::<SessionNotification>(notif.params.get()) else {
        tracing::warn!("Failed to parse {}", notif.method.as_ref());
        return false;
    };
    match &session_notif.update {
        XaiSessionUpdate::TaskBackgrounded { .. } => {
            return handle_task_backgrounded(notif, app);
        }
        XaiSessionUpdate::TaskCompleted { .. } => {
            return handle_task_completed(notif, app);
        }
        XaiSessionUpdate::ScheduledTaskCreated { .. } => {
            return handle_scheduled_task_created(notif, app);
        }
        XaiSessionUpdate::ScheduledTaskDeleted { .. } => {
            return handle_scheduled_task_deleted(notif, app);
        }
        _ => {}
    }
    let is_api_key_auth = app.is_api_key_auth;
    let matched = match find_session_match(app, &session_notif.session_id) {
        Some(m) => m,
        None => {
            tracing::debug!(
                session_id = session_notif.session_id.0.as_ref(),
                method = notif.method.as_ref(),
                "load-race: x.ai/session_notification DROPPED — no agent matches session_id"
            );
            return false;
        }
    };
    let parent_id = matched.agent_id();
    let is_active = is_matched_agent_active(app, parent_id);
    let agent = app
        .agents
        .get_mut(&parent_id)
        .expect("find_session_match returned an existing AgentId");
    if matches!(matched, SessionMatch::Child(_)) {
        let child_sid: &str = session_notif.session_id.0.as_ref();
        let changed = handle_child_session_notification(
            session_notif.update,
            child_sid,
            agent,
            is_api_key_auth,
        );
        return changed && is_active;
    }
    let meta = NotificationMeta::from_json(session_notif.meta.as_ref().and_then(|v| v.as_object()));
    if drop_unexpected_replay(
        agent,
        &meta,
        session_notif.session_id.0.as_ref(),
        "x.ai/session/update",
    ) {
        return false;
    }
    let is_workflow_update = matches!(
        session_notif.update,
        XaiSessionUpdate::WorkflowUpdated { .. }
    );
    if !is_workflow_update
        && !meta.is_replay
        && meta.event_seq.is_some_and(|seq| {
            agent
                .last_applied_xai_event_seq
                .is_some_and(|last| seq <= last)
        })
    {
        tracing::debug!(
            session_id = session_notif.session_id.0.as_ref(),
            event_seq = meta.event_seq,
            last_applied = agent.last_applied_xai_event_seq,
            "x.ai/session update DROPPED by dedup highwater (event_seq <= last_applied)"
        );
        return false;
    }
    let mut plugins_changed_needs_skills_refetch = false;
    let mut terminal_outcome: Option<super::super::turn_completion::TerminalApply> = None;
    let root_session_id: &str = session_notif.session_id.0.as_ref();
    let changed = match session_notif.update {
        ref update @ (XaiSessionUpdate::AutoCompactStarted { .. }
        | XaiSessionUpdate::AutoCompactCompleted { .. }
        | XaiSessionUpdate::AutoCompactFailed { .. }
        | XaiSessionUpdate::AutoCompactCancelled { .. }
        | XaiSessionUpdate::RetryState(_)
        | XaiSessionUpdate::ImageDropped { .. }
        | XaiSessionUpdate::MemoryFlushCompleted { .. }
        | XaiSessionUpdate::MemoryDreamCompleted { .. }
        | XaiSessionUpdate::MemorySessionSaved { .. }) => {
            let changed = apply_session_event(
                update,
                &mut agent.session,
                &mut agent.scrollback,
                is_api_key_auth,
            );
            if let XaiSessionUpdate::AutoCompactCompleted { tokens_after, .. } = update {
                refresh_context_used(agent, *tokens_after);
                agent.todo.update_todos(Vec::new());
            }
            changed
        }
        XaiSessionUpdate::ImageCompressed {
            ref images,
            ref message,
        } => apply_image_compressed(agent, images, message),
        XaiSessionUpdate::TurnCompleted {
            prompt_id,
            stop_reason,
            agent_result,
            ..
        } => {
            if agent.session.loading_replay {
                agent.replayed_terminal_prompts.insert(prompt_id);
                false
            } else if is_wake_prompt(&prompt_id) {
                if agent.session.state.is_busy() {
                    false
                } else {
                    finish_wake_turn(agent);
                    true
                }
            } else {
                let cancel_trigger = session_notif
                    .meta
                    .as_ref()
                    .and_then(|v| v.get("cancelTrigger"))
                    .and_then(|v| v.as_str());
                terminal_outcome =
                    Some(super::super::turn_completion::finalize_turn_from_terminal(
                        agent,
                        root_session_id,
                        Some(&prompt_id),
                        Some(&stop_reason),
                        agent_result.as_deref(),
                        cancel_trigger,
                    ));
                false
            }
        }
        XaiSessionUpdate::SubagentSpawned {
            subagent_id,
            child_session_id,
            subagent_type,
            description,
            persona,
            role,
            model,
            effective_context_source,
            resumed_from,
            capability_mode,
            context_normalized,
            parent_prompt_id,
            workflow_run_id,
            ..
        } => {
            tracing::info!(
                child_session_id = % child_session_id, subagent_type = % subagent_type,
                "Subagent spawned"
            );
            let is_background = agent
                .session
                .tracker
                .task_tool_background
                .remove(&subagent_id)
                .unwrap_or(false);
            let persona_display = persona.clone();
            let role_display = role.clone();
            let model_display = model.clone();
            agent.subagent_sessions.insert(
                child_session_id.clone(),
                SubagentInfo {
                    subagent_id: Arc::from(subagent_id),
                    child_session_id: Arc::from(child_session_id.clone()),
                    description: Arc::from(description.clone()),
                    subagent_type: Arc::from(subagent_type.clone()),
                    persona: persona.map(Arc::from),
                    role: role.map(Arc::from),
                    model: model.map(Arc::from),
                    context_source: effective_context_source.map(Arc::from),
                    resumed_from: resumed_from.map(Arc::from),
                    capability_mode: capability_mode.map(Arc::from),
                    workflow_run_id: workflow_run_id.clone().map(Arc::from),
                    context_normalized,
                    parent_prompt_id: parent_prompt_id.map(Arc::from),
                    started_at: std::time::Instant::now(),
                    last_progress_at: std::time::Instant::now(),
                    finished: false,
                    status: None,
                    error: None,
                    duration_ms: None,
                    tool_calls: None,
                    turns: None,
                    turn_count: None,
                    tool_call_count: None,
                    tokens_used: None,
                    context_window_tokens: None,
                    context_usage_pct: None,
                    tools_used: Vec::new(),
                    error_count: None,
                    activity_label: None,
                    is_background,
                    pending_kill: false,
                    kill_requested_at: None,
                    scrollback_entry_id: None,
                    prompt: None,
                    child_cwd: None,
                    worktree_path: None,
                    child_updates_replayed: false,
                },
            );
            if let Some(ref sid) = agent.session.session_id
                && let Some(info) = agent.subagent_sessions.get_mut(&child_session_id)
            {
                crate::app::subagent::enrich_from_meta(info, &agent.session.cwd, sid.0.as_ref());
            }
            let (effective_child_cwd, effective_is_worktree) = derive_child_cwd(
                &agent.session.cwd,
                agent.subagent_sessions.get(&child_session_id),
            );
            let child_session = AgentSession {
                id: AgentId(0),
                acp_tx: agent.session.acp_tx.clone(),
                session_id: Some(acp::SessionId::new(child_session_id.clone())),
                models: agent.session.models.clone(),
                state: AgentState::TurnRunning,
                tracker: AcpUpdateTracker::new(),
                cwd: effective_child_cwd,
                is_worktree: effective_is_worktree,
                forked_from: None,
                pending_prompts: std::collections::VecDeque::new(),
                next_queue_id: 0,
                yolo_mode: true,
                auto_mode: false,
                prompt_history: Vec::new(),
                prompt_history_loading: false,
                loading_replay: false,
                restore_degree: None,
                rate_limited: false,
                model_incompatible: false,
                credit_limit_blocked: false,
                free_usage_blocked: false,
                bg_tasks: std::collections::BTreeMap::new(),
                bg_tool_call_to_task: std::collections::HashMap::new(),
                scheduled_tasks: std::collections::HashMap::new(),
                available_commands: Vec::new(),
                available_commands_generation: 0,
                available_tools: None,
                model_switch_pending: false,
                user_model_preference: None,
                deferred_model_switch: None,
                in_flight_prompt: None,
                current_prompt_id: None,
                created_via_new: false,
            };
            let mut child_scrollback = crate::scrollback::state::ScrollbackState::new();
            child_scrollback.set_appearance(agent.scrollback.appearance().clone());
            let mut child_view = AgentView::new(child_session, child_scrollback);
            child_view.set_input_mode(InputMode::Vim);
            child_view.active_pane = crate::views::agent::ActivePane::Scrollback;
            child_view.set_sharing_enabled(agent.sharing_enabled);
            child_view.set_billing_surface_visible(agent.billing_surface_visible);
            let dashboard_visible = agent
                .prompt
                .slash_controller
                .registry()
                .get("dashboard")
                .is_some();
            child_view.set_dashboard_visible(dashboard_visible);
            child_view.set_has_session_announcements(
                agent.prompt.slash_controller.has_session_announcements(),
            );
            child_view
                .prompt
                .set_screen_mode(agent.prompt.slash_controller.screen_mode());
            child_view.app_chat_mode = agent.app_chat_mode;
            let recap_visible = agent
                .prompt
                .slash_controller
                .registry()
                .get("recap")
                .is_some();
            child_view.set_session_recap_available(recap_visible);
            let voice_visible = agent
                .prompt
                .slash_controller
                .registry()
                .get("voice")
                .is_some();
            child_view.set_voice_mode_available(voice_visible);
            let restricted = agent
                .prompt
                .slash_controller
                .registry()
                .restricted_commands();
            child_view.set_restricted_commands(&restricted);
            agent.insert_subagent_view(child_session_id.clone(), Box::new(child_view));
            if !agent.session.loading_replay {
                if let Some(child_view) = agent.subagent_views.get_mut(&child_session_id) {
                    crate::app::subagent::replay_inherited_updates(child_view, &child_session_id);
                }
                if let Some(info) = agent.subagent_sessions.get_mut(&child_session_id) {
                    info.child_updates_replayed = true;
                }
            }
            let prompt_to_inject = agent
                .subagent_sessions
                .get(&child_session_id)
                .and_then(|info| info.prompt.as_deref())
                .filter(|p| !p.trim().is_empty())
                .filter(|p| {
                    agent
                        .subagent_views
                        .get(&child_session_id)
                        .is_some_and(|cv| {
                            !crate::app::subagent::child_scrollback_already_shows_prompt(
                                &cv.scrollback,
                                p,
                            )
                        })
                })
                .map(str::to_owned);
            if let (Some(prompt), Some(child_view)) = (
                prompt_to_inject,
                agent.subagent_views.get_mut(&child_session_id),
            ) {
                child_view
                    .scrollback
                    .push_block(RenderBlock::user_prompt(prompt));
                child_view.session.tracker.expect_user_echo();
            }
            if workflow_run_id.is_none() {
                let block = crate::scrollback::blocks::SubagentBlock::started(
                    &description,
                    &child_session_id,
                    &subagent_type,
                    persona_display,
                    role_display,
                    model_display,
                    is_background,
                );
                let entry_id = agent.scrollback.push_block(RenderBlock::Subagent(block));
                agent.scrollback.set_last_running(true);
                if let Some(info) = agent.subagent_sessions.get_mut(&child_session_id) {
                    info.scrollback_entry_id = Some(entry_id);
                    info.is_background = is_background;
                }
                agent.maybe_push_parked_marker();
            } else if let Some(info) = agent.subagent_sessions.get_mut(&child_session_id) {
                info.is_background = is_background;
            }
            true
        }
        XaiSessionUpdate::SubagentProgress {
            child_session_id,
            duration_ms,
            turn_count,
            tool_call_count,
            tokens_used,
            context_window_tokens,
            context_usage_pct,
            tools_used,
            error_count,
            ..
        } => {
            if let Some(info) = agent.subagent_sessions.get_mut(&child_session_id) {
                info.duration_ms = Some(duration_ms);
                info.turn_count = Some(turn_count);
                info.tool_call_count = Some(tool_call_count);
                info.tokens_used = Some(tokens_used);
                info.context_window_tokens = Some(context_window_tokens);
                info.context_usage_pct = Some(context_usage_pct);
                info.tools_used = tools_used.into_iter().map(Arc::from).collect();
                info.error_count = Some(error_count);
                info.last_progress_at = std::time::Instant::now();
            }
            if let Some(child_view) = agent.subagent_views.get_mut(&child_session_id)
                && context_window_tokens > 0
            {
                child_view
                    .session
                    .models
                    .override_context_window(context_window_tokens);
            }
            let activity_label = agent
                .subagent_views
                .get(&child_session_id)
                .and_then(|cv| subagent_activity_label(cv));
            sync_subagent_activity(agent, &child_session_id, activity_label);
            true
        }
        XaiSessionUpdate::SubagentFinished {
            child_session_id,
            status,
            error,
            tool_calls,
            turns,
            duration_ms,
            tokens_used,
            ..
        } => {
            tracing::info!(
                child_session_id = % child_session_id, status = % status, tool_calls =
                tool_calls, turns = turns, duration_ms = duration_ms, "Subagent finished"
            );
            let elapsed_dur = std::time::Duration::from_millis(duration_ms);
            let info_ref = agent.subagent_sessions.get(&child_session_id);
            let entry_id = info_ref.and_then(|s| s.scrollback_entry_id);
            let is_background = info_ref.is_some_and(|s| s.is_background);
            let description = info_ref.map(|s| s.description.clone()).unwrap_or_default();
            if let Some(eid) = entry_id {
                agent.scrollback.finish_running(eid);
            }
            sync_subagent_activity(agent, &child_session_id, None);
            if is_background {
                let block = match status.as_str() {
                    "completed" => {
                        RenderBlock::Subagent(crate::scrollback::blocks::SubagentBlock::completed(
                            description.as_ref(),
                            child_session_id.as_str(),
                            elapsed_dur,
                        ))
                    }
                    "cancelled" => {
                        RenderBlock::Subagent(crate::scrollback::blocks::SubagentBlock::cancelled(
                            description.as_ref(),
                            child_session_id.as_str(),
                            elapsed_dur,
                        ))
                    }
                    _ => RenderBlock::Subagent(crate::scrollback::blocks::SubagentBlock::failed(
                        description.as_ref(),
                        child_session_id.as_str(),
                        elapsed_dur,
                        error.clone(),
                    )),
                };
                agent.scrollback.push_block(block);
            } else if let Some(eid) = entry_id
                && let Some(entry) = agent.scrollback.get_by_id_mut(eid)
            {
                if let RenderBlock::Subagent(ref mut sb) = entry.block {
                    match status.as_str() {
                        "completed" => {
                            sb.kind = crate::scrollback::blocks::SubagentBlockKind::Completed {
                                elapsed: elapsed_dur,
                            };
                        }
                        "cancelled" => {
                            sb.kind = crate::scrollback::blocks::SubagentBlockKind::Cancelled {
                                elapsed: elapsed_dur,
                            };
                        }
                        _ => {
                            sb.kind = crate::scrollback::blocks::SubagentBlockKind::Failed {
                                elapsed: elapsed_dur,
                                error: error.clone(),
                            };
                        }
                    }
                }
                entry.invalidate_cache();
            }
            if let Some(info) = agent.subagent_sessions.get_mut(&child_session_id) {
                info.finished = true;
                info.status = Some(Arc::from(status));
                info.error = error.map(Arc::from);
                info.duration_ms = Some(duration_ms);
                info.tool_calls = Some(tool_calls);
                info.turns = Some(turns);
                if tokens_used > 0 {
                    info.tokens_used = Some(tokens_used);
                }
                info.pending_kill = false;
                info.kill_requested_at = None;
                info.last_progress_at = std::time::Instant::now();
            }
            let resuming = agent.session.loading_replay;
            if let Some(child_view) = agent.subagent_views.get_mut(&child_session_id) {
                child_view.session.state = AgentState::Idle;
                if !resuming {
                    crate::app::subagent::finalize_finished_child_view(child_view, elapsed_dur);
                }
            }
            if !resuming {
                agent.maybe_push_parked_marker();
            }
            true
        }
        XaiSessionUpdate::HookAnnotation { message } => {
            if app.appearance.disable_plugins {
                return false;
            }
            tracing::debug!("Hook annotation: {message}");
            agent
                .scrollback
                .push_block(RenderBlock::session_event(SessionEvent::HookAnnotation {
                    message,
                }));
            true
        }
        XaiSessionUpdate::HookExecution {
            event_name,
            tool_name: _tool_name,
            prompt_id: batch_prompt_id,
            runs,
        } => {
            use crate::scrollback::blocks::tool::{HookPhase, HookRunEntry, HookRunStatus};
            let hook_entries: Vec<HookRunEntry> = runs
                .into_iter()
                .map(|r| {
                    let status = match r.status {
                        xai_grok_shell::extensions::notification::HookRunStatusDto::Success {
                            elapsed_ms,
                        } => HookRunStatus::Success {
                            elapsed: std::time::Duration::from_millis(elapsed_ms),
                        },
                        xai_grok_shell::extensions::notification::HookRunStatusDto::Skipped => {
                            HookRunStatus::Skipped
                        }
                        xai_grok_shell::extensions::notification::HookRunStatusDto::Failed {
                            error,
                            elapsed_ms,
                            blocked: true,
                        } => HookRunStatus::Blocked {
                            detail: error,
                            elapsed: std::time::Duration::from_millis(elapsed_ms),
                        },
                        xai_grok_shell::extensions::notification::HookRunStatusDto::Failed {
                            error,
                            elapsed_ms,
                            blocked: false,
                        } => HookRunStatus::Failed {
                            error,
                            elapsed: std::time::Duration::from_millis(elapsed_ms),
                        },
                    };
                    HookRunEntry {
                        name: r.name,
                        status,
                        output: r.output,
                    }
                })
                .collect();
            let is_tool_hook = event_name == "pre_tool_use" || event_name == "post_tool_use";
            let is_stop_hook = event_name == "stop" || event_name == "stop_failure";
            if is_tool_hook {
                let phase = if event_name == "pre_tool_use" {
                    HookPhase::Pre
                } else {
                    HookPhase::Post
                };
                if let Some(entry_id) = agent.scrollback.last_tool_call_entry_id() {
                    agent.scrollback.attach_hooks(entry_id, phase, hook_entries);
                }
            } else if is_stop_hook && !meta.is_replay && !agent.session.loading_replay {
                let local_turn_active =
                    agent.session.state.is_turn_running() || agent.session.state.is_cancelling();
                let batch_is_wake = batch_prompt_id.as_deref().is_some_and(is_wake_prompt);
                let foreign_batch = batch_prompt_id.is_some()
                    && agent.session.current_prompt_id.is_some()
                    && batch_prompt_id != agent.session.current_prompt_id
                    && !batch_is_wake;
                if foreign_batch {
                    agent
                        .scrollback
                        .push_lifecycle_hooks(event_name, hook_entries);
                } else if !batch_is_wake && local_turn_active {
                    let stash_pid = batch_prompt_id
                        .clone()
                        .or_else(|| agent.session.current_prompt_id.clone());
                    stash_live_stop_batch(
                        agent,
                        stash_pid,
                        event_name,
                        hook_entries,
                        batch_prompt_id.is_some(),
                    );
                } else if let Some(entry_id) = agent
                    .scrollback
                    .latest_turn_marker_accepting(&event_name, batch_prompt_id.as_deref())
                {
                    agent.scrollback.attach_stop_hooks_to_marker(
                        entry_id,
                        event_name,
                        hook_entries,
                        batch_prompt_id.as_deref(),
                    );
                } else {
                    agent
                        .scrollback
                        .push_lifecycle_hooks(event_name, hook_entries);
                }
            } else {
                agent
                    .scrollback
                    .push_lifecycle_hooks(event_name, hook_entries);
            }
            true
        }
        XaiSessionUpdate::HooksChanged {
            hooks,
            project_trusted,
            load_errors,
        } => {
            if let Some(ref mut modal) = agent.extensions_modal {
                use crate::views::extensions_modal::TabDataState;
                modal.hooks_data =
                    TabDataState::Loaded(xai_hooks_plugins_types::HooksListResponse {
                        hooks,
                        project_trusted,
                        load_errors,
                    });
                true
            } else {
                false
            }
        }
        XaiSessionUpdate::PluginsChanged { plugins } => {
            if let Some(ref mut modal) = agent.extensions_modal {
                use crate::views::extensions_modal::TabDataState;
                modal.seed_plugin_groups_once(&plugins);
                modal.plugins_data =
                    TabDataState::Loaded(xai_hooks_plugins_types::PluginsListResponse { plugins });
                if !matches!(modal.skills_data, TabDataState::Loading) {
                    modal.skills_data = TabDataState::Loading;
                    plugins_changed_needs_skills_refetch = true;
                }
                true
            } else {
                false
            }
        }
        XaiSessionUpdate::SessionSummaryGenerated { session_summary } => {
            agent.generated_session_title =
                Some(crate::util::decode_html_entities(&session_summary).into_owned());
            true
        }
        XaiSessionUpdate::SessionRecap { summary, auto } => {
            use crate::scrollback::block::RenderBlock;
            use crate::scrollback::blocks::SessionEvent;
            if should_drop_late_auto_recap(auto, meta.is_replay, agent.session.state.is_idle()) {
                tracing::debug!(
                    "dropping late auto SessionRecap; agent busy (turn or command in flight)"
                );
                false
            } else {
                app.notification_service.focus_tracker.mark_recap_shown();
                let recap_block = RenderBlock::session_event(SessionEvent::Recap { summary, auto });
                apply_recap_block(agent, auto, recap_block);
                true
            }
        }
        XaiSessionUpdate::SessionRecapUnavailable => {
            if meta.is_replay {
                false
            } else if let Some(pending_id) = agent.pending_recap_entry.take() {
                agent.scrollback.remove_entry(pending_id);
                agent.show_toast(crate::app::dispatch::recap_unavailable_toast(
                    crate::app::dispatch::scrollback_has_user_messages(&agent.scrollback),
                ));
                true
            } else {
                false
            }
        }
        XaiSessionUpdate::ModelAutoSwitched {
            previous_model_id,
            new_model_id,
            reason,
        } => {
            use crate::scrollback::block::RenderBlock;
            use crate::scrollback::blocks::SessionEvent;
            let available_count = agent.session.models.available.len();
            let available_keys: Vec<&str> = agent
                .session
                .models
                .available
                .keys()
                .take(10)
                .map(|m| m.0.as_ref())
                .collect();
            tracing::warn!(
                session_id = session_notif.session_id.0.as_ref(), previous = %
                previous_model_id, new = % new_model_id, available_count, available_keys
                = ? available_keys,
                "Model auto-switched: previous model no longer available"
            );
            crate::unified_log::warn(
                "model auto-switched: previous model unavailable",
                Some(session_notif.session_id.0.as_ref()),
                Some(serde_json::json!(
                    { "previous_model" : previous_model_id.as_str(), "new_model" :
                    new_model_id.as_str(), "available_count" : available_count,
                    "available_keys" : available_keys, }
                )),
            );
            agent.scrollback.push_block(RenderBlock::session_event(
                SessionEvent::ModelUnavailable {
                    previous_model_id,
                    new_model_id,
                    reason,
                },
            ));
            true
        }
        XaiSessionUpdate::ModelChanged {
            model_id,
            reasoning_effort,
        } => {
            if agent.session.model_switch_pending {
                tracing::debug!(
                    session_id = session_notif.session_id.0.as_ref(), model_id = %
                    model_id,
                    "ignoring ModelChanged broadcast — local switch is in flight"
                );
                return false;
            }
            use xai_grok_shell::sampling::types::ReasoningEffort;
            let new_model_id = acp::ModelId::new(model_id.clone());
            if !agent.session.models.available.contains_key(&new_model_id) {
                if xai_grok_shell::agent::chat_modes::process_chat_mode_enabled() {
                    agent.session.models.available.insert(
                        new_model_id.clone(),
                        acp::ModelInfo::new(new_model_id.clone(), model_id.clone()),
                    );
                } else {
                    tracing::warn!(
                        session_id = session_notif.session_id.0.as_ref(), model_id = %
                        model_id,
                        "ignoring ModelChanged broadcast — model not in local catalog"
                    );
                    return false;
                }
            }
            let effort = reasoning_effort
                .as_deref()
                .and_then(|s| s.parse::<ReasoningEffort>().ok());
            let prev_model = agent.session.models.current.clone();
            let prev_effort = agent.session.models.reasoning_effort;
            agent
                .session
                .models
                .set_current(new_model_id.clone(), effort);
            agent.session.user_model_preference = Some(new_model_id.clone());
            let resolved_effort = agent.session.models.reasoning_effort;
            let actually_changed =
                prev_model.as_ref() != Some(&new_model_id) || prev_effort != resolved_effort;
            if actually_changed {
                tracing::info!(
                    session_id = session_notif.session_id.0.as_ref(), model_id = %
                    model_id, effort = ? resolved_effort,
                    "ModelChanged broadcast applied (remote switch)"
                );
            }
            actually_changed
        }
        XaiSessionUpdate::MemoryFiles { files } => {
            let entries = crate::views::memory_modal::build_entries(files);
            let modal_state = crate::views::memory_modal::MemoryModalState::new(entries);
            agent.active_modal = Some(crate::views::modal::ActiveModal::MemoryBrowser {
                state: Box::new(modal_state),
            });
            true
        }
        update @ XaiSessionUpdate::WorkflowUpdated { .. } => ingest_workflow_update(agent, update),
        XaiSessionUpdate::GoalUpdated {
            goal_id,
            objective,
            status,
            phase,
            token_budget,
            tokens_used,
            elapsed_ms,
            total_deliverables,
            completed_deliverables,
            current_deliverable_id,
            current_deliverable_title,
            current_subagent_role,
            total_worker_rounds,
            total_verify_rounds,
            token_baseline,
            finished_subagent_tokens,
            live_subagent_tokens,
            live_tokens_by_model,
            live_context_pct,
            live_turn_count,
            live_tool_call_count,
            last_event,
            last_event_detail,
            last_event_timestamp,
            pause_message,
            classifier_runs_attempted,
            classifier_max_runs,
            last_classifier_verdict,
            last_classifier_details_path,
            verifying_completion,
            planning,
            ..
        } => {
            let new_status = GoalDisplayStatus::parse(&status);
            let just_completed = new_status == GoalDisplayStatus::Complete
                && agent
                    .goal_state
                    .as_ref()
                    .is_none_or(|g| g.status != GoalDisplayStatus::Complete);
            if status == "cleared" {
                if let Some(g) = agent.goal_state.take() {
                    agent.last_cleared_goal_id = Some(g.goal_id);
                }
                agent.show_goal_detail = false;
                true
            } else if agent.last_cleared_goal_id.as_deref() == Some(goal_id.as_str()) {
                false
            } else {
                let elapsed_floor_ms = agent
                    .goal_state
                    .as_ref()
                    .filter(|g| g.goal_id == goal_id)
                    .map(|g| g.live_elapsed_ms())
                    .unwrap_or(0)
                    .max(elapsed_ms);
                if just_completed {
                    agent.scrollback.push_block(RenderBlock::session_event(
                        SessionEvent::GoalCompleted {
                            elapsed: std::time::Duration::from_millis(elapsed_floor_ms),
                        },
                    ));
                }
                let last_classifier_details_exists = last_classifier_details_path
                    .as_deref()
                    .is_some_and(|p| std::path::Path::new(p).exists());
                agent.goal_state = Some(GoalDisplayState {
                    goal_id,
                    objective,
                    status: new_status,
                    phase: GoalDisplayPhase::parse(&phase),
                    token_budget,
                    tokens_used,
                    elapsed_ms,
                    total_deliverables,
                    completed_deliverables,
                    current_deliverable_id,
                    current_deliverable_title,
                    current_subagent_role,
                    total_worker_rounds,
                    total_verify_rounds,
                    live_subagent_tokens,
                    live_tokens_by_model,
                    live_context_pct,
                    live_turn_count,
                    live_tool_call_count,
                    last_event,
                    last_event_detail,
                    last_event_timestamp,
                    token_baseline,
                    finished_subagent_tokens,
                    deliverables: Vec::new(),
                    pause_message,
                    classifier_runs_attempted,
                    classifier_max_runs,
                    last_classifier_verdict,
                    last_classifier_details_path,
                    last_classifier_details_exists,
                    verifying_completion: verifying_completion.unwrap_or(false),
                    planning: planning.unwrap_or(false),
                    received_at: std::time::Instant::now(),
                    elapsed_floor_ms,
                });
                true
            }
        }
        XaiSessionUpdate::InteractionResolved { tool_call_id } => {
            agent.dismiss_resolved_interaction(&tool_call_id)
        }
        _ => {
            tracing::trace!(
                "Ignoring {}: {:?}",
                notif.method.as_ref(),
                std::mem::discriminant(&session_notif.update)
            );
            return false;
        }
    };
    if plugins_changed_needs_skills_refetch {
        if let Some(agent) = app.agents.get(&parent_id)
            && let Some(session_id) = agent.session.session_id.clone()
        {
            app.pending_effects.push(Effect::FetchSkillsList {
                agent_id: parent_id,
                session_id,
            });
        } else if let Some(agent) = app.agents.get_mut(&parent_id)
            && let Some(ref mut modal) = agent.extensions_modal
        {
            modal.skills_data =
                crate::views::extensions_modal::TabDataState::Error("No active session".into());
        } else {
            tracing::warn!("PluginsChanged: agent or modal disappeared before skills re-fetch");
        }
    }
    if let Some(agent) = app.agents.get_mut(&parent_id) {
        if let Some(seq) = meta.event_seq
            && !meta.is_replay
            && !is_workflow_update
        {
            agent.last_applied_xai_event_seq = Some(seq);
        }
        if let Some(id) = meta.event_id {
            agent.last_seen_event_id = Some(id);
        }
    }
    if let Some(outcome) = terminal_outcome {
        return super::super::turn_completion::apply_terminal_outcome(
            outcome, app, parent_id, is_active,
        );
    }
    changed && is_active
}
/// Handle an xAI session notification that targets a child (subagent) session.
///
/// Events like compaction, retry, and memory flush are emitted by the child's
/// `acp_session` with the *child's* `session_id`. This routes them to the
/// correct child view and updates `SubagentInfo` where appropriate.
pub(super) fn handle_child_session_notification(
    update: XaiSessionUpdate,
    child_sid: &str,
    agent: &mut AgentView,
    is_api_key_auth: bool,
) -> bool {
    match update {
        XaiSessionUpdate::AutoCompactStarted { .. }
        | XaiSessionUpdate::AutoCompactCompleted { .. }
        | XaiSessionUpdate::AutoCompactFailed { .. }
        | XaiSessionUpdate::AutoCompactCancelled { .. }
        | XaiSessionUpdate::RetryState(_) => {
            let compact_tokens = match &update {
                XaiSessionUpdate::AutoCompactCompleted { tokens_after, .. } => Some(*tokens_after),
                _ => None,
            };
            let mut changed = false;
            if let Some(child_view) = agent.subagent_views.get_mut(child_sid) {
                changed = apply_session_event(
                    &update,
                    &mut child_view.session,
                    &mut child_view.scrollback,
                    is_api_key_auth,
                );
                if let Some(tokens_after) = compact_tokens {
                    refresh_context_used(child_view, tokens_after);
                }
            }
            if let Some(tokens_after) = compact_tokens
                && let Some(info) = agent.subagent_sessions.get_mut(child_sid)
            {
                info.tokens_used = Some(tokens_after);
                if let Some(cw) = info.context_window_tokens.filter(|&cw| cw > 0) {
                    info.context_usage_pct =
                        Some(xai_token_estimation::usage_percentage_u8(tokens_after, cw));
                }
            }
            changed
        }
        ref update @ (XaiSessionUpdate::MemoryFlushCompleted { .. }
        | XaiSessionUpdate::MemoryDreamCompleted { .. }
        | XaiSessionUpdate::MemorySessionSaved { .. }) => {
            if let Some(child_view) = agent.subagent_views.get_mut(child_sid) {
                apply_session_event(
                    update,
                    &mut child_view.session,
                    &mut child_view.scrollback,
                    is_api_key_auth,
                )
            } else {
                false
            }
        }
        _ => false,
    }
}
/// Apply a compaction or retry event to a session's activity state and scrollback.
///
/// Shared between the root agent and child (subagent) notification paths.
/// Test-only shim so dispatch-level tests can replay real notification
/// sequences (e.g. `RetryState::Retrying` → `Exhausted`) through the
/// production handler — the Retrying arm clears the `in_flight_prompt`
/// rewind stash, which a fixture setting fields directly would miss.
#[cfg(test)]
pub(crate) fn apply_session_event_for_test(
    update: &XaiSessionUpdate,
    session: &mut AgentSession,
    scrollback: &mut crate::scrollback::state::ScrollbackState,
) -> bool {
    apply_session_event(update, session, scrollback, false)
}
pub(super) fn apply_session_event(
    update: &XaiSessionUpdate,
    session: &mut AgentSession,
    scrollback: &mut crate::scrollback::state::ScrollbackState,
    is_api_key_auth: bool,
) -> bool {
    match update {
        XaiSessionUpdate::AutoCompactStarted { percentage, .. } => {
            tracing::info!("Auto-compact started: {percentage}% context used");
            session.in_flight_prompt = None;
            session.set_compaction_activity(Some(TurnActivity::AutoCompacting));
            scrollback.push_block(RenderBlock::session_event(
                SessionEvent::CompactionStarted {
                    percentage: *percentage,
                },
            ));
            true
        }
        XaiSessionUpdate::AutoCompactCompleted {
            tokens_before,
            tokens_after,
            elapsed_ms,
            ..
        } => {
            tracing::info!("Auto-compact completed: {tokens_after} tokens after");
            session.set_compaction_activity(None);
            if session.loading_replay {
                scrollback.push_block(RenderBlock::session_event(
                    SessionEvent::CompactionCompleted {
                        tokens_before: *tokens_before,
                        tokens_after: *tokens_after,
                        elapsed_ms: *elapsed_ms,
                    },
                ));
            } else {
                session.defer_compaction(*tokens_before, *tokens_after, *elapsed_ms);
            }
            true
        }
        XaiSessionUpdate::AutoCompactFailed { error } => {
            tracing::error!(error = % error, "Auto-compaction failed");
            session.set_compaction_activity(None);
            scrollback.push_block(RenderBlock::session_event(SessionEvent::CompactionFailed {
                error: error.clone(),
            }));
            true
        }
        XaiSessionUpdate::AutoCompactCancelled { .. } => {
            tracing::info!("Auto-compact cancelled");
            session.set_compaction_activity(None);
            scrollback.push_block(RenderBlock::session_event(
                SessionEvent::CompactionCancelled,
            ));
            true
        }
        XaiSessionUpdate::RetryState(retry) => {
            tracing::debug!("Retry state: {retry:?}");
            apply_retry_state(retry, session, scrollback, is_api_key_auth);
            true
        }
        XaiSessionUpdate::ImageDropped { notes } => {
            let message = notes.join("\n");
            tracing::info!("Image dropped: {message}");
            scrollback.push_block(RenderBlock::system(message));
            true
        }
        _ => false,
    }
}
/// True if the trailing run of session/system blocks contains a
/// [`SessionEvent::CompactionFailed`]. Used so we don't stack a [`SessionEvent::ContextTooLarge`]
/// prompt on top of the compaction handler's "too large to compact" message.
pub(super) fn scrollback_has_recent_compaction_failed(
    scrollback: &crate::scrollback::state::ScrollbackState,
) -> bool {
    use crate::scrollback::block::RenderBlock;
    for idx in (0..scrollback.len()).rev() {
        match scrollback.entry(idx).map(|e| &e.block) {
            Some(RenderBlock::SessionEvent(ev)) => {
                if matches!(ev.event, SessionEvent::CompactionFailed { .. }) {
                    return true;
                }
            }
            Some(RenderBlock::System(_)) => {}
            _ => break,
        }
    }
    false
}
/// Handle an `ImageCompressed` notification. A successful compression is
/// deliberately invisible in the TUI (log-only): it needs no user action,
/// and the model-facing `<image_compression_notice>` reminder is attached
/// to the prompt independently. Only the re-encode *fallback* — the
/// oversized original was KEPT — surfaces, as a persistent scrollback
/// warning (and is re-materialized on session replay).
pub(super) fn apply_image_compressed(
    agent: &mut AgentView,
    images: &[xai_grok_shell::extensions::notification::ImageCompressedEntry],
    message: &str,
) -> bool {
    if images.is_empty() {
        tracing::warn!("Image re-encode fallback: {message}");
        agent
            .scrollback
            .push_block(RenderBlock::system(message.to_owned()));
        return true;
    }
    tracing::info!("Image compressed: {message}");
    false
}
pub(super) fn apply_retry_state(
    retry: &xai_grok_shell::extensions::notification::RetryState,
    session: &mut AgentSession,
    scrollback: &mut crate::scrollback::state::ScrollbackState,
    is_api_key_auth: bool,
) {
    let mut is_credit_limit = false;
    let mut is_reauth = false;
    use xai_grok_shell::extensions::notification::RetryState;
    match retry {
        RetryState::Retrying {
            attempt,
            max_retries,
            reason,
        } => {
            session.set_retry_activity(Some(TurnActivity::Retrying {
                attempt: *attempt,
                max_retries: *max_retries,
                reason: reason.clone(),
            }));
        }
        RetryState::Exhausted {
            attempts,
            reason,
            is_rate_limited: rate_limited,
        } => {
            session.set_retry_activity(None);
            session.rate_limited = *rate_limited;
            if *rate_limited {
                xai_grok_telemetry::session_ctx::log_event(
                    xai_grok_telemetry::events::RateLimitHit {
                        model_id: session
                            .models
                            .current
                            .as_ref()
                            .map(|m| m.0.to_string())
                            .unwrap_or_default(),
                        attempts: *attempts,
                    },
                );
            }
            is_credit_limit = super::super::dispatch::is_credit_limit_error(None, reason);
            let is_free_usage = *rate_limited
                && xai_grok_shell::sampling::error::is_free_usage_exhausted_error(reason);
            if is_credit_limit {
                session.credit_limit_blocked = true;
            } else if is_free_usage {
                session.free_usage_blocked = true;
            } else if !*rate_limited && is_reauthable_failure(None, reason) {
                is_reauth = true;
                scrollback.push_block(RenderBlock::session_event(SessionEvent::ReAuthRequired));
            } else {
                let error = if *rate_limited {
                    crate::app::effects::sanitize_user_error(&format_rate_limited_user_message(
                        Some(reason.as_str()),
                        is_api_key_auth,
                    ))
                } else {
                    format!("failed after {attempts} retries: {reason}")
                };
                scrollback.push_block(RenderBlock::session_event(SessionEvent::RetryFailed {
                    error,
                    error_type: None,
                }));
            }
        }
        RetryState::Failed {
            error_type,
            message,
        } => {
            session.set_retry_activity(None);
            if error_type == "encrypted_content_mismatch" {
                session.model_incompatible = true;
            }
            is_credit_limit = super::super::dispatch::is_credit_limit_error(None, message);
            if is_credit_limit {
                session.credit_limit_blocked = true;
            } else if is_reauthable_failure(Some(error_type.as_str()), message) {
                is_reauth = true;
                scrollback.push_block(RenderBlock::session_event(SessionEvent::ReAuthRequired));
            } else if error_type == "context_length" {
                if !scrollback_has_recent_compaction_failed(scrollback) {
                    scrollback
                        .push_block(RenderBlock::session_event(SessionEvent::ContextTooLarge));
                }
            } else {
                scrollback.push_block(RenderBlock::session_event(SessionEvent::RetryFailed {
                    error: message.clone(),
                    error_type: Some(error_type.clone()),
                }));
            }
        }
    }
    if is_credit_limit {
        xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::CreditLimitHit {
            model_id: session
                .models
                .current
                .as_ref()
                .map(|m| m.0.to_string())
                .unwrap_or_default(),
        });
    } else if !is_reauth {
        session.in_flight_prompt = None;
    }
}
/// Single source of truth for plan-mode state on the pager side.
///
/// The agent emits `CurrentModeUpdate` on every entry and exit — both for
/// user-driven mode switches (Shift+Tab → `session/set_mode`) and for
/// agent-driven `EnterPlanMode` / `ExitPlanMode` tool calls (mapped by the
/// notification bridge).
///
/// Do not be tempted to infer mode from tool-call titles: titles incorporate
/// raw model/user input (Grep pattern, Bash command, search query, ...), so
/// a substring match silently bricks sessions whenever any tool happens to
/// mention `enter_plan_mode`.
///
/// Returns `true` when a `CurrentModeUpdate` was processed so the
/// caller can refresh open settings modals after the per-agent borrow
/// releases.
pub(super) fn detect_plan_mode_change(update: &acp::SessionUpdate, agent: &mut AgentView) -> bool {
    use xai_grok_tools::types::SessionMode;
    let acp::SessionUpdate::CurrentModeUpdate(cmu) = update else {
        return false;
    };
    let mode = SessionMode::from_id(cmu.current_mode_id.0.as_ref());
    let was_active = agent.plan_mode_active;
    let now_active = mode.is_plan();
    agent.plan_mode_active = now_active;
    agent.plan_mode_pending = None;
    if was_active != now_active {
        tracing::info!(
            mode_id = % cmu.current_mode_id.0, plan_active = now_active,
            "Plan mode state updated (from CurrentModeUpdate)"
        );
    }
    true
}
