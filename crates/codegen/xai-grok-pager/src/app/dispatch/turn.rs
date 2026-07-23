//! Turn cancellation, task and subagent kills, and overdue turn reconciliation.

use super::ctx::find_agent_by_session_id;
use super::permissions::drain_permission_queue;
use super::queue::{apply_turn_start_shim, maybe_drain_queue, note_peek_page_flip};
use crate::app::actions::Effect;
use crate::app::agent::AgentId;
use crate::app::agent_view::ActivePane;
use crate::app::app_view::{ActiveView, AppView};
use crate::scrollback::blocks::SessionEvent;
use std::time::Instant;

/// Map `[ui].cancel_subagents_on_turn_cancel` / in-memory agent preference to
/// `cancel_subagents` for the cancel wire payload. `None` means prompt.
fn effective_cancel_subagents_preference(
    agent_pref: Option<bool>,
    ui: &xai_grok_shell::agent::config::UiConfig,
) -> Option<bool> {
    agent_pref.or(match ui.cancel_subagents_on_turn_cancel.as_deref() {
        Some("always_stop") => Some(true),
        Some("always_continue") => Some(false),
        _ => None,
    })
}

fn cancel_subagents_pref_canonical(stop: bool) -> &'static str {
    if stop {
        "always_stop"
    } else {
        "always_continue"
    }
}

fn cancel_subagents_pref_canonical_from_ui(
    ui: &xai_grok_shell::agent::config::UiConfig,
) -> &'static str {
    match ui.cancel_subagents_on_turn_cancel.as_deref() {
        Some("always_stop") => "always_stop",
        Some("always_continue") => "always_continue",
        _ => "ask",
    }
}

/// Apply a global always-stop / always-continue preference to every agent and
/// `app.current_ui` (in-memory only; caller emits `Effect::PersistSetting`).
pub(super) fn apply_cancel_subagents_preference_global(app: &mut AppView, stop: bool) {
    let canonical = cancel_subagents_pref_canonical(stop);
    app.current_ui.cancel_subagents_on_turn_cancel = Some(canonical.to_string());
    for agent in app.agents.values_mut() {
        agent.cancel_subagents_preference = Some(stop);
    }
}

pub(super) fn dispatch_cancel_turn(app: &mut AppView) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let ui_pref = effective_cancel_subagents_preference(None, &app.current_ui);

    // Scoped agent borrow: extract decisions, then release before `do_cancel_turn`.
    let preferred_cancel_subagents = {
        let Some(agent) = app.agents.get_mut(&id) else {
            return vec![];
        };
        let resolved_pref = agent.cancel_subagents_preference.or(ui_pref);
        // Retry path: a cancel was already sent (`TurnCancelling`) but the turn
        // never resolved — the `session/cancel` notification or the turn-end
        // response may have been lost in transit. Re-send instead of silently
        // no-opping (cancel is idempotent on the agent), so Ctrl+C / palette
        // CancelTurn is never a dead key on a stuck "Cancelling…" spinner.
        // Skips the subagent panel — that
        // choice was already made (or defaulted) on the first cancel.
        if agent.session.state.is_cancelling() {
            let Some(session_id) = agent.session.session_id.clone() else {
                return vec![];
            };
            crate::unified_log::info(
                "cancel.retry",
                Some(&session_id.0),
                Some(serde_json::json!({
                    "current_prompt_id": agent.session.current_prompt_id,
                })),
            );
            // Explicit user cancel supersedes any pending send-now expectation (its marker renders).
            agent.clear_send_now_expectation();
            return vec![Effect::CancelTurn {
                session_id,
                cancel_subagents: resolved_pref.unwrap_or(true),
                // A fresh gesture (e.g. a second Ctrl+C on a stuck spinner) re-set
                // the hint; consume it so the re-sent cancel still carries the trigger.
                trigger: agent.cancel_trigger_hint.take(),
                // Retry cancel of a stuck turn — no local prompt rewind here.
                rewind_if_pristine: false,
            }];
        }
        if !agent.session.state.is_turn_running() {
            return vec![];
        }
        if let Some(stop) = resolved_pref {
            Some(stop)
        } else {
            // Check all running subagents, not just those from the current turn.
            // This is broader than the old TUI (which filtered by parent_prompt_id),
            // but intentional: subagents kept alive from a previous cancel should
            // still prompt the user on the next cancel.
            let running_count = agent
                .subagent_sessions
                .values()
                .filter(|s| s.is_running() && s.workflow_run_id.is_none())
                .count();
            if running_count > 0 && agent.cancel_turn_view.is_none() {
                agent.cancel_turn_view = Some(crate::views::modal::CancelTurnViewState {
                    active_idx: 0,
                    running_count,
                });
                // Default focus to the picker so keyboard up/down navigates options
                // immediately. Without this, if the user triggered cancel while the
                // scrollback pane was focused (e.g. browsing history), the modal
                // would open but keystrokes would still go to scrollback — the
                // picker was only reachable via mouse hover/click.
                if agent.active_pane == ActivePane::Scrollback {
                    agent.active_pane = ActivePane::Prompt;
                }
                return vec![];
            }
            None
        }
    };

    do_cancel_turn(app, preferred_cancel_subagents.unwrap_or(true))
}

pub(super) fn dispatch_cancel_turn_choice(
    app: &mut AppView,
    choice: crate::views::modal::CancelTurnChoice,
) -> Vec<Effect> {
    use crate::views::modal::CancelTurnChoice;
    let cancel_subagents = matches!(
        choice,
        CancelTurnChoice::StopRunning | CancelTurnChoice::AlwaysStop
    );

    if let ActiveView::Agent(id) = app.active_view
        && let Some(agent) = app.agents.get_mut(&id)
    {
        agent.cancel_turn_view = None;
        agent.cancel_turn_buttons.clear();
    }

    let mut effects = Vec::new();
    match choice {
        CancelTurnChoice::AlwaysStop | CancelTurnChoice::AlwaysContinue => {
            let stop = matches!(choice, CancelTurnChoice::AlwaysStop);
            let prev_canonical = cancel_subagents_pref_canonical_from_ui(&app.current_ui);
            let new_canonical = cancel_subagents_pref_canonical(stop);
            apply_cancel_subagents_preference_global(app, stop);
            if prev_canonical != new_canonical {
                tracing::info!(
                    target: "settings",
                    key = "cancel_subagents_on_turn_cancel",
                    value = new_canonical,
                    "setting changed",
                );
                effects.push(Effect::PersistSetting {
                    key: "cancel_subagents_on_turn_cancel",
                    value: crate::settings::SettingValue::Enum(new_canonical),
                    rollback_value: crate::settings::SettingValue::Enum(prev_canonical),
                });
            }
        }
        // One-shot choices: apply only to this cancel; global/session pref unchanged.
        CancelTurnChoice::StopRunning | CancelTurnChoice::ContinueToRun => {}
    }

    effects.extend(do_cancel_turn(app, cancel_subagents));
    effects
}

pub(super) fn do_cancel_turn(app: &mut AppView, cancel_subagents: bool) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    if !agent.session.state.is_turn_running() {
        return vec![];
    }
    // If the server hasn't emitted any activity yet AND there are no other
    // queued prompts, "rewind" the prompt back into the input box and remove
    // its scrollback block. The cancel notification still flies to the
    // server, but the local turn state is reset to Idle immediately so the
    // UI looks like the user never hit Send.
    //
    // Skip rewind when queued prompts exist: restoring the in-flight prompt
    // to the input box while the next queued prompt drains would mix two
    // user intentions in confusing ways. Fall back to the standard cancel
    // flow in that case.
    //
    // Clearing `current_prompt_id` (via `finish_turn`) is what makes orphan
    // chunks/PR for the cancelled turn get dropped by the `promptId` gate
    // in acp_handler / PromptResponse handler.
    // When a prompt is queued on the server-authoritative shared queue, cancel
    // restores the FRONT queued prompt to the input instead (handled after the
    // cleanup below). So skip the in-flight rewind in that case — the user wants
    // the queued prompt back, not the in-flight one.
    //
    // Minimal mode prints each committed block once into the terminal's native
    // scrollback, and that print can't be "un-printed". A user-prompt block
    // commits immediately (it is never `is_running`), so a just-promoted queued
    // prompt's block is already in native scrollback by the time the user can
    // cancel it. Rewinding then `remove_entry`s it from scrollback *state* while
    // the printed copy stays on screen AND restores the text into the input —
    // showing the prompt twice (dogfood bug: double-Esc on a queued prompt). Skip
    // the rewind when the in-flight block has already committed and fall back to
    // the standard cancel. `committed` is always false in alt-screen / inline, so
    // this is a no-op outside minimal.
    let in_flight_committed = match agent.session.in_flight_prompt.as_ref() {
        Some(stashed) => agent.scrollback.is_committed(stashed.scrollback_entry),
        None => false,
    };
    // The rewind REPLACES the composer with the stashed in-flight prompt.
    // Esc (and the mouse stop / palette cancel) fire with the draft intact —
    // unlike keyboard Ctrl+C, which only cancels on an empty prompt — so a
    // non-empty composer holds a NEWER draft the rewind would clobber.
    // Trigger-agnostic on purpose: fall back to the standard cancel.
    let composer_has_draft = !agent.prompt.text().is_empty() || !agent.prompt.images.is_empty();
    let rewinding = agent.shared_queue.is_empty()
        && app.cancel_rewind_enabled
        && agent.session.in_flight_prompt.is_some()
        && agent.session.pending_prompts.is_empty()
        && !in_flight_committed
        && !composer_has_draft;
    if rewinding && let Some(stashed) = agent.session.in_flight_prompt.take() {
        if let Some(pid) = agent.session.current_prompt_id.clone() {
            agent.note_rewound_prompt(&pid);
        }
        agent.prompt.set_text(&stashed.text);
        agent.prompt.restore_chip_elements(&stashed.chip_elements);
        agent.prompt.set_images(stashed.images);
        agent.prompt.set_cursor(stashed.text.len());
        for id in stashed.combined_scrollback_entries {
            agent.scrollback.remove_entry(id);
        }
        agent.scrollback.remove_entry(stashed.scrollback_entry);
        // Full state reset: tracker cleanup + state Idle + clear timing
        // fields + clear current_prompt_id.
        agent.session.finish_turn(&mut agent.scrollback);
        agent.turn_started_at = None;
        agent.activity_started_at = None;
        agent.last_activity = None;
    } else {
        agent.session.cancel_turn(&mut agent.scrollback);
    }
    agent.cancel_turn_view = None;
    agent.cancel_turn_buttons.clear();
    drain_permission_queue(agent);
    if let Some(mut pav) = agent.plan_approval_view.take() {
        pav.send_stale_cancel();
        agent.plan_next_comment_id = pav.next_comment_id;
        agent.prompt.restore(pav.stashed_prompt);
        agent.line_viewer = None;
    }

    let Some(session_id) = agent.session.session_id.clone() else {
        return vec![];
    };

    // Explicit user cancel supersedes any pending send-now expectation (its marker renders).
    agent.clear_send_now_expectation();

    // Server-authoritative queue: the agent owns the drain. On an interactive
    // cancel we only tear down the running turn and let the agent promote the
    // FRONT queued prompt as the next turn — its `x.ai/queue/changed`
    // rebroadcast (carrying `running_prompt_id`) is the source of truth, and the
    // pager adopts it via `handle_queue_changed` / `apply_turn_start_shim`. We
    // do NOT pull any queued prompt back into the input or predict the new queue
    // order client-side; the user's first queued prompt is what runs next.
    vec![Effect::CancelTurn {
        session_id,
        cancel_subagents,
        // Consume the gesture hint set by the key/mouse handler (persists
        // through the subagent picker until this final build). `None` for
        // non-gesture callers (login/reauth flows).
        trigger: agent.cancel_trigger_hint.take(),
        // Mirror the local rewind on the wire: when we restored the prompt to
        // the composer above, ask the shell to trim its pristine copy too so a
        // resend can't pair the kept copy with the new send.
        rewind_if_pristine: rewinding,
    }]
}

/// Grace window between a driver-side `x.ai/session/prompt_complete`
/// broadcast and that turn's `session/prompt` RPC response, after which
/// [`reconcile_overdue_turn_ends`] finishes the turn from the broadcast. The
/// healthy-path gap is milliseconds (the shell emits the broadcast just
/// before writing the RPC response), so an expiry means the response is
/// genuinely lost, not merely slow.
pub(crate) const TURN_END_RECONCILE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Finish turns whose end was announced by `x.ai/session/prompt_complete`
/// but whose `session/prompt` RPC response never arrived.
///
/// The RPC response is the driver's only turn-state exit, and it can be lost
/// in leader response routing / reconnect races (the loss left the TUI
/// latched in `TurnCancelling` — Esc dead, prompts piling into a queue
/// that never drains — until a restart). The
/// broadcast is armed in `handle_prompt_complete` and disarmed by a matching
/// `TaskResult::PromptResponse`; whatever is still armed past
/// [`TURN_END_RECONCILE_GRACE`] is reconciled here with the essential subset
/// of the PromptResponse teardown (state, marker, adoption hand-off, queue
/// drain).
///
/// Returns `None` when nothing fired; `Some(effects)` (possibly empty) when
/// at least one agent was reconciled, so the caller forces a redraw.
pub(crate) fn reconcile_overdue_turn_ends(app: &mut AppView) -> Option<Vec<Effect>> {
    let overdue: Vec<AgentId> = app
        .agents
        .iter()
        .filter(|(_, a)| {
            a.pending_turn_end_reconcile
                .as_ref()
                .is_some_and(|p| p.received_at.elapsed() >= TURN_END_RECONCILE_GRACE)
        })
        .map(|(id, _)| *id)
        .collect();
    if overdue.is_empty() {
        return None;
    }

    let mut fired = false;
    let mut effects = Vec::new();
    let mut drained_ids = Vec::new();
    for id in overdue {
        // Take the stashed adoption before borrowing the agent (disjoint
        // `app` fields; same pattern as the PromptResponse arm).
        let pending_adoption = app.pending_running_adoptions.remove(&id);
        let Some(agent) = app.agents.get_mut(&id) else {
            continue;
        };
        let Some(pending) = agent.pending_turn_end_reconcile.take() else {
            continue;
        };

        let still_ours =
            agent.session.current_prompt_id.as_deref() == Some(pending.prompt_id.as_str());
        let busy = agent.session.state.is_turn_running() || agent.session.state.is_cancelling();
        if !still_ours || !busy {
            // The turn already resolved through the normal path (or a new
            // turn was adopted); the marker is stale. Restore the adoption
            // for the path that owns it.
            if let Some(p) = pending_adoption {
                app.pending_running_adoptions.insert(id, p);
            }
            continue;
        }

        fired = true;
        let was_cancelling = agent.session.state.is_cancelling()
            || pending.stop_reason.as_deref() == Some("cancelled");
        // Send-now cancel: suppress the marker (wire `cancelTrigger` wins, else
        // the armed expectation). Consumed every reconcile (no stale flag).
        let expected_send_now = agent.expect_send_now_cancel.take();
        let send_now_cancel = was_cancelling
            && match pending.cancel_trigger.as_deref() {
                Some(trigger) => trigger == "send_now",
                None => expected_send_now.is_some(),
            };
        let elapsed = agent.turn_elapsed().unwrap_or_default();
        crate::unified_log::warn(
            "turn.end_reconciled_from_broadcast",
            agent.session.session_id.as_ref().map(|s| s.0.as_ref()),
            Some(serde_json::json!({
                "prompt_id": pending.prompt_id,
                "stop_reason": pending.stop_reason,
                "was_cancelling": was_cancelling,
                "send_now_cancel": send_now_cancel,
                "grace_ms": TURN_END_RECONCILE_GRACE.as_millis() as u64,
            })),
        );

        agent.session.finish_turn(&mut agent.scrollback);
        let event = if was_cancelling {
            // Send-now cancel renders no marker (the new prompt is the next turn).
            (!send_now_cancel).then_some(SessionEvent::TurnCancelled { elapsed })
        } else {
            match pending.stop_reason.as_deref() {
                // Rate limits drive a dedicated driver UX via the retry
                // notifications (already delivered); no extra marker.
                Some("rate_limit") => None,
                Some("error") => Some(SessionEvent::TurnFailed {
                    error: pending
                        .agent_result
                        .clone()
                        .unwrap_or_else(|| "unknown error".into()),
                    elapsed: Some(elapsed),
                }),
                _ => Some(SessionEvent::TurnCompleted {
                    elapsed: Some(elapsed),
                }),
            }
        };
        crate::app::turn_completion::push_turn_terminal_marker(
            agent,
            event,
            Some(pending.prompt_id.as_str()),
        );

        agent.mark_turn_finished();
        agent.activity_started_at = None;
        agent.last_activity = None;
        drain_permission_queue(agent);
        agent.cancel_turn_view = None;
        agent.cancel_turn_buttons.clear();
        if agent.bash_turn {
            agent.bash_turn = false;
            agent.scrollback.goto_bottom();
        }
        agent.cron_task_id = None;

        // FIFO handoff (mirrors the PromptResponse arm): adopt the next
        // server-authoritative running prompt now that the slot is free.
        let adopted_page_flip = if let Some(p) = pending_adoption
            && agent.session.current_prompt_id.is_none()
        {
            if p.prompt_id != pending.prompt_id && agent.should_adopt_running_prompt(&p.prompt_id) {
                apply_turn_start_shim(agent, p.prompt_id, p.text, &p.kind, p.combined_texts)
            } else {
                agent.discard_pending_adoption_updates(&p.prompt_id);
                None
            }
        } else {
            None
        };
        let drain = maybe_drain_queue(agent);
        effects.extend(drain.effects);
        drained_ids.push((id, adopted_page_flip.or(drain.page_flip_entry)));
    }
    for (id, page_flip_entry) in drained_ids {
        note_peek_page_flip(app, id, page_flip_entry);
    }
    fired.then_some(effects)
}

pub(super) fn dispatch_cancel_scheduled_task(app: &mut AppView, task_id: String) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        return vec![];
    };

    // Remove from local state immediately (optimistic).
    agent.session.scheduled_tasks.remove(&task_id);

    vec![Effect::DeleteScheduledTask {
        session_id,
        task_id,
    }]
}

pub(super) fn dispatch_kill_bg_task(app: &mut AppView, task_id: String) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        return vec![];
    };

    // Mark as pending_kill for UI feedback
    if let Some(task) = agent.session.bg_tasks.get_mut(&task_id) {
        task.pending_kill = true;
        task.kill_requested_at = Some(Instant::now());
    }

    vec![Effect::KillBgTask {
        session_id,
        task_id,
    }]
}

pub(super) fn dispatch_kill_subagent(app: &mut AppView, subagent_id: String) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        return vec![];
    };

    // Mark as pending_kill for UI feedback
    for info in agent.subagent_sessions.values_mut() {
        if info.subagent_id.as_ref() == subagent_id {
            info.pending_kill = true;
            info.kill_requested_at = Some(Instant::now());
        }
    }

    vec![Effect::KillSubagent {
        session_id,
        subagent_id,
    }]
}

pub(super) fn dispatch_demote_to_background(app: &mut AppView) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    if !agent.session.state.is_turn_running() {
        return vec![];
    }
    let Some(session_id) = agent.session.session_id.clone() else {
        return vec![];
    };
    // Get the tool_call_id of the currently running execute tool
    let Some(tool_call_id) = agent
        .session
        .tracker
        .running_execute_tool_call_id()
        .map(|s| s.to_string())
    else {
        return vec![];
    };

    tracing::info!(tool_call_id = %tool_call_id, "Demoting execute tool to background");

    vec![Effect::DemoteToBackground {
        session_id,
        tool_call_id,
    }]
}

// TODO: Add dispatch_cancel_command() once xai-grok-shell supports proper
// server-side cancellation for /compact. Currently, the compaction handler
// uses spawn_local with no cancellation token, and blindly replaces the
// conversation history when done — so prompts sent after a client-side
// cancel would be lost.

// TaskResult handlers.

pub(super) fn handle_bg_task_killed(
    app: &mut AppView,
    session_id: String,
    task_id: String,
    outcome: Option<xai_grok_tools::types::KillOutcome>,
) -> Vec<Effect> {
    use xai_grok_tools::types::KillOutcome;
    if let Some(agent) = find_agent_by_session_id(&mut app.agents, &session_id) {
        match outcome {
            Some(KillOutcome::Killed) => {
                // Stay in pending_kill state — task_completed notification
                // will arrive and clear it.
                tracing::info!(task_id = %task_id, "Kill signal sent");
            }
            Some(KillOutcome::AlreadyExited) => {
                if let Some(task) = agent.session.bg_tasks.get_mut(&task_id) {
                    task.pending_kill = false;
                    task.kill_requested_at = None;
                }
            }
            Some(KillOutcome::NotFound) => {
                // Stale row (e.g. restored from a resume replay but the
                // process belongs to a previous session lifetime): the
                // agent has nothing to kill, so drop the row and finish
                // its "Task started" scrollback entry (stops the
                // running accent that the replay restore turned on).
                tracing::info!(task_id = %task_id, "Task not found, removing");
                if let Some(task) = agent.session.bg_tasks.remove(&task_id)
                    && let Some(entry_id) = task.scrollback_entry_id
                {
                    agent.scrollback.finish_running(entry_id);
                }
            }
            None => {
                // Error envelope or unparseable payload: clear the
                // pending state so the user can retry, keep the row.
                tracing::warn!(task_id = %task_id, "Kill outcome missing or unparseable");
                if let Some(task) = agent.session.bg_tasks.get_mut(&task_id) {
                    task.pending_kill = false;
                    task.kill_requested_at = None;
                }
            }
        }
    }
    vec![]
}
