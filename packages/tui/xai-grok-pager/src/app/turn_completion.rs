//! Finalizing a turn from a terminal turn signal.
//!
//! The pager learns a turn reached its terminal outcome from two rails: the
//! fire-and-forget `x.ai/session/prompt_complete` broadcast (the one-release
//! compat path for not-yet-upgraded leaders) and the durable, persisted+replayed
//! `XaiSessionUpdate::TurnCompleted`. Both converge on
//! [`finalize_turn_from_terminal`] so the turn-finalize behavior lives in one
//! place — and so a viewer that re-attaches mid-turn can finalize the turn from
//! replay instead of staying stuck on "Waiting…".

use crate::scrollback::blocks::SessionEvent;

use super::agent::AgentId;
use super::agent_view::AgentView;
use super::app_view::AppView;
use super::cancel_latency::TurnEnd;

/// `_meta.cancellationCategory` of a hook-denied turn end: renders the
/// "blocked by a hook" marker instead of "cancelled by user" on every rail.
pub(crate) const HOOK_DENIED_CATEGORY: &str =
    xai_grok_shell::session::commands::HOOK_DENIED_CATEGORY;

/// `_meta` key of a cancelled terminal's trigger (`"send_now"`, `"ctrl_c"`, …).
pub(crate) const CANCEL_TRIGGER_KEY: &str = "cancelTrigger";
/// `_meta` key of a terminal's cancellation category (e.g.
/// [`HOOK_DENIED_CATEGORY`]).
pub(crate) const CANCELLATION_CATEGORY_KEY: &str = "cancellationCategory";
/// `_meta` key of a cancelled terminal's structured detail (hook name,
/// reason). Stamped beside the category; absent on older shells.
pub(crate) const CANCELLATION_CONTEXT_KEY: &str = "cancellationContext";

/// Unknown tokens stay [`TurnStopReason::Unknown`] → `TurnCompleted` (live `_` arm).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnStopReason {
    EndTurn,
    Cancelled,
    Refusal,
    RateLimit,
    Error,
    MaxTokens,
    MaxTurnRequests,
    Unknown,
}

impl From<&str> for TurnStopReason {
    fn from(s: &str) -> Self {
        match s {
            "end_turn" => Self::EndTurn,
            "cancelled" => Self::Cancelled,
            "refusal" => Self::Refusal,
            "rate_limit" => Self::RateLimit,
            "error" => Self::Error,
            "max_tokens" => Self::MaxTokens,
            "max_turn_requests" => Self::MaxTurnRequests,
            _ => Self::Unknown,
        }
    }
}

impl From<Option<&str>> for TurnStopReason {
    fn from(s: Option<&str>) -> Self {
        s.map(Self::from).unwrap_or(Self::Unknown)
    }
}

pub(crate) struct TerminalMarkerInput<'a> {
    pub stop: TurnStopReason,
    pub elapsed_ms: Option<u64>,
    pub agent_result: Option<&'a str>,
    pub send_now_cancel: bool,
    pub cancellation_category: Option<&'a str>,
    pub error_banner_present: bool,
}

/// Saturating `Duration` → ms. Missing stays `None`. Shared by every
/// `terminal_marker` caller so the copies cannot drift.
pub(crate) fn duration_to_elapsed_ms(elapsed: Option<std::time::Duration>) -> Option<u64> {
    elapsed.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn required_elapsed(ms: Option<u64>) -> std::time::Duration {
    // Missing wire elapsed: Duration::ZERO so cancel/hook markers still render.
    ms.map(std::time::Duration::from_millis)
        .unwrap_or(std::time::Duration::ZERO)
}

pub(crate) fn terminal_marker(input: TerminalMarkerInput<'_>) -> Option<SessionEvent> {
    let elapsed = input.elapsed_ms.map(std::time::Duration::from_millis);
    match input.stop {
        TurnStopReason::EndTurn
        | TurnStopReason::Refusal
        | TurnStopReason::MaxTokens
        | TurnStopReason::MaxTurnRequests
        | TurnStopReason::Unknown => Some(SessionEvent::TurnCompleted { elapsed }),
        TurnStopReason::Cancelled if input.send_now_cancel => None,
        TurnStopReason::Cancelled => Some(cancelled_turn_event(
            input.cancellation_category,
            required_elapsed(input.elapsed_ms),
        )),
        TurnStopReason::RateLimit => None,
        TurnStopReason::Error if input.error_banner_present => None,
        TurnStopReason::Error => {
            let raw = input.agent_result.unwrap_or("unknown error");
            Some(SessionEvent::TurnFailed {
                error: crate::app::error_display::format_request_failure(None, None, raw).message(),
                elapsed,
            })
        }
    }
}

/// The turn-cancelled terminal marker for a cancel of `category`: the
/// hook-denied category renders [`SessionEvent::TurnBlockedByHook`], anything
/// else the user-cancel copy. One chooser for all rails so the wording can't
/// drift between the driver, viewer, reconcile, and wake paths.
pub(super) fn cancelled_turn_event(
    cancellation_category: Option<&str>,
    elapsed: std::time::Duration,
) -> SessionEvent {
    if cancellation_category == Some(HOOK_DENIED_CATEGORY) {
        SessionEvent::TurnBlockedByHook { elapsed }
    } else {
        SessionEvent::TurnCancelled { elapsed }
    }
}

/// Every finalize rail (driver, viewer, reconcile, wake) must route through
/// here so none can miss arming the hold. The blocked prompt requeues at the
/// queue FRONT: a fixed resubmission must run ahead of held followers.
pub(super) fn note_hook_blocked_turn(
    agent: &mut AgentView,
    prompt_id: Option<&str>,
    cancellation_category: Option<&str>,
    cancellation_context: Option<&serde_json::Value>,
) {
    if cancellation_category != Some(HOOK_DENIED_CATEGORY) {
        return;
    }
    agent.session.hook_block_hold = true;
    // A blocked turn never broadcasts its user echo: consume the armed skip
    // so it cannot swallow the next live user message (another client's
    // prompt).
    agent.session.tracker.clear_user_echo_skip();
    // The scrollback bubble stays: the live session keeps showing what the
    // user typed, even though a blocked prompt is stored nowhere.
    //
    // Turn adoption stashes a text-only rewind restore for foreign turns too;
    // only the originating client may requeue the prompt and own the card.
    let foreign = prompt_id.is_some_and(|p| !agent.is_self_originated_prompt(p));
    let mut requeued = None;
    let mut was_combined = false;
    if !foreign && let Some(stashed) = agent.session.in_flight_prompt.take() {
        was_combined = !stashed.combined_scrollback_entries.is_empty();
        let text = stashed.text.clone();
        let id = agent.session.enqueue_in_flight_prompt_front(stashed);
        requeued = Some((id, text));
    }
    let held = agent.session.pending_prompts.len();
    crate::unified_log::info(
        "prompt.hook_block_hold_armed",
        agent.session.session_id.as_ref().map(|s| s.0.as_ref()),
        Some(serde_json::json!({
            "queue_depth": held,
            "requeued_front": requeued.is_some(),
            "has_context": cancellation_context.is_some(),
        })),
    );
    match requeued {
        Some((row_id, text)) => {
            // One parse point for the wire context: the shell serializes
            // `CancellationContext` (camelCase), and this deserializes into
            // the same shared type — no stringly key lookups.
            let ctx: Option<xai_grok_shell::session::commands::CancellationContext> =
                cancellation_context.and_then(|v| serde_json::from_value(v.clone()).ok());
            let blocked = crate::app::agent::BlockedPromptContext {
                row_id,
                hook_name: ctx.as_ref().and_then(|c| c.hook_name.clone()),
                reason: ctx.and_then(|c| c.reason),
                was_combined,
            };
            agent.session.blocked_prompt = Some(blocked.clone());
            open_prompt_blocked_card(agent, &blocked, text);
        }
        // A hold with no requeued row (foreign turn, or no stash) has no
        // card; the toast keeps the parked queue from being silent.
        None => agent.show_toast("A hook blocked the last prompt — the queue is paused"),
    }
}

/// Reopen the blocked-prompt card from the stored context, after a
/// queue-edit exit that resolved nothing (Esc, pane switch, save or delete
/// of an unrelated row). Returns whether the card is up.
pub(in crate::app) fn reopen_blocked_card_if_held(agent: &mut AgentView) -> bool {
    if !agent.session.hook_block_hold {
        return false;
    }
    let Some(blocked) = agent.session.blocked_prompt.clone() else {
        return false;
    };
    let Some(text) = agent
        .session
        .pending_prompts
        .iter()
        .find(|p| p.id == blocked.row_id)
        .map(|p| p.text.clone())
    else {
        return false;
    };
    open_prompt_blocked_card(agent, &blocked, text);
    agent.question_view.is_some()
}

fn open_prompt_blocked_card(
    agent: &mut AgentView,
    blocked: &crate::app::agent::BlockedPromptContext,
    prompt_text: String,
) {
    use crate::views::question_view::{LocalQuestionKind, QuestionViewState};
    use xai_grok_tools::implementations::grok_build::ask_user_question::{
        Question, QuestionOption,
    };

    if agent.question_view.is_some()
        || matches!(
            agent.prompt_mode,
            crate::app::queue_edit::PromptMode::EditingQueued { .. }
        )
    {
        // Modal collision or a composer busy with a queue edit. The hold, the
        // requeued row, and the stored context (for a later reopen) already
        // protect the queue; leave a plain toast so the parked state isn't
        // silent.
        agent.show_toast("Prompt blocked by a hook — it is held at the front of the queue");
        return;
    }

    let row_id = blocked.row_id;
    let was_combined = blocked.was_combined;
    let hook_name = blocked.hook_name.as_deref().unwrap_or("a hook");
    // The qualified name carries the config path (`global/block-hihi:
    // user_prompt_submit[0].hooks[0]`); the part before the first `:` is
    // enough to identify the hook on the card.
    let short_hook_name = hook_name.split(':').next().unwrap_or(hook_name);
    let reason = blocked.reason.as_deref().unwrap_or_default();

    // `\n\n` splits the card header into a bold label plus dimmed
    // description lines (one per paragraph): framing, hook reason verbatim,
    // queue context.
    let mut question = format!("Prompt blocked by {short_hook_name}");
    if !reason.is_empty() {
        question.push_str("\n\n");
        question.push_str(reason);
    }
    // Waiting rows sit BEHIND the requeued blocked prompt.
    let waiting = agent.session.pending_prompts.len().saturating_sub(1);
    if was_combined {
        question.push_str("\n\nThis was a combined submission.");
    }
    if waiting > 0 {
        question.push_str(&format!(
            "\n\n{waiting} more prompt{} waiting (queue paused).",
            if waiting == 1 { "" } else { "s" }
        ));
    }

    let preview = Some(prompt_text);
    let options = vec![
        QuestionOption {
            label: "Edit".into(),
            description: "Fix your prompt".into(),
            preview: preview.clone(),
            id: None,
        },
        QuestionOption {
            label: "Resend".into(),
            description: "Send it unchanged. The hook may block it again.".into(),
            preview: preview.clone(),
            id: None,
        },
        QuestionOption {
            label: "Discard".into(),
            description: "Remove it from the queue".into(),
            preview,
            id: None,
        },
    ];

    let stashed = agent.prompt.stash();
    agent.question_view = Some(
        QuestionViewState::new(
            format!("prompt-blocked-{row_id}"),
            vec![Question {
                question,
                id: None,
                options,
                multi_select: Some(false),
            }],
            stashed,
        )
        .with_local_kind(LocalQuestionKind::PromptBlocked { row_id })
        .with_no_freeform(),
    );
    agent.prompt.set_text("");
}

/// Push a turn-terminal marker ("Turn completed/cancelled/failed"), folding
/// any pending stop-family hook runs into it so they render inline
/// (right-justified) on the marker line instead of as a standalone block.
///
/// All three marker rails route through here: the driver's `PromptResponse`,
/// the lost-RPC reconcile, and the viewer finalize. (Wake turns route through
/// `finish_wake_turn` in acp_handler, which maps their stop reason and calls
/// here only when a marker is due.) `event == None`
/// (bash turns, rate-limit / re-auth UX that replaces the marker) flushes the
/// held hooks as the legacy standalone lifecycle block so failures stay
/// visible.
///
/// A stamped stash folds only on an exact ending-id match. On a mismatch it
/// flushes standalone (the ending turn is THE turn — an older stash has no
/// marker coming). An unstamped stash keeps the legacy
/// stashed-during-this-turn heuristic.
pub(super) fn push_turn_terminal_marker(
    agent: &mut AgentView,
    event: Option<SessionEvent>,
    ending_prompt_id: Option<&str>,
) {
    let pending = agent.pending_stop_hooks.take();
    let groups = match pending {
        None => Vec::new(),
        Some(pending) => {
            let stale = match (pending.prompt_id.as_deref(), ending_prompt_id) {
                (Some(stashed), Some(ending)) => stashed != ending,
                (Some(_), None) => true,
                (None, _) => false,
            };
            if stale {
                for (name, runs) in pending.groups {
                    agent.scrollback.push_lifecycle_hooks(name, runs);
                }
                Vec::new()
            } else {
                pending.groups
            }
        }
    };

    match event {
        Some(event) => {
            agent.push_end_marker_block(event, groups, ending_prompt_id.map(str::to_string));
        }
        None => {
            for (name, runs) in groups {
                agent.scrollback.push_lifecycle_hooks(name, runs);
            }
        }
    }
}

/// A turn-terminal signal's wire fields (`turn_completed` params + `_meta`,
/// or the legacy `prompt_complete` payload); parks 1:1 onto
/// [`PendingTurnEnd`](super::agent_view::PendingTurnEnd) when the driver arms
/// the lost-RPC reconcile.
// Test-only Default: production call sites must name every wire field.
#[derive(Clone, Copy)]
#[cfg_attr(test, derive(Default))]
pub(super) struct TerminalSignal<'a> {
    /// The ended turn's `promptId`, when the broadcast carried one.
    pub prompt_id: Option<&'a str>,
    /// `stopReason` (`"cancelled"`, `"end_turn"`, …).
    pub stop_reason: Option<&'a str>,
    /// `agentResult` detail (error text, when present).
    pub agent_result: Option<&'a str>,
    /// `_meta.cancelTrigger`: `"send_now"` is the silent half of a
    /// cancel-and-send, so the `TurnCancelled` marker is suppressed. Absent
    /// meta means a normal cancel, unless this client just dispatched the
    /// send-now (`AgentView::expect_send_now_cancel`, older-shell fallback).
    pub cancel_trigger: Option<&'a str>,
    /// `_meta.cancellationCategory`: `"HookDenied"` picks the
    /// blocked-by-a-hook marker. Absent on older shells and plain user
    /// cancels.
    pub cancellation_category: Option<&'a str>,
    /// `_meta.cancellationContext`: structured detail of a hook-denied end
    /// (hook name, reason), shown on the blocked-prompt card. Absent on older
    /// shells and non-hook cancels.
    pub cancellation_context: Option<&'a serde_json::Value>,
}

/// What applying a terminal turn signal did to one agent.
pub(super) enum TerminalApply {
    /// No change: a driver turn the signal does not provably match, or a
    /// duplicate/stale terminal for an already-finished viewer turn.
    Ignored,
    /// Driver: the lost-RPC reconcile was armed. The turn is NOT finished — the
    /// `PromptResponse` RPC owns the driver's lifecycle. Reported as a state
    /// change so the reconcile sweep's animation tick stays scheduled.
    ReconcileArmed,
    /// Viewer: the turn was finished and (for non-rate-limit reasons) a terminal
    /// marker pushed. The caller drops any stale running-prompt adoption.
    ViewerFinalized,
}

/// Arm lost-`PromptResponse` reconcile for the driver turn we own.
///
/// - **Exact** `prompt_id` match → arm (canonical).
/// - **Missing** wire `promptId` (`None` or empty) → arm on `current_prompt_id`
///   only when the turn is not mid-tool/thinking/compact/retry (legacy /
///   broken `TurnCompleted` payloads).
/// - **Non-empty mismatch** → ignore (stale/peer terminal must not kill a
///   newer live turn after grace).
///
/// Never clobber an existing arm for a different pid; keep earliest
/// `received_at` when re-arming the same pid.
fn arm_driver_turn_end_reconcile(
    agent: &mut AgentView,
    session_id: &str,
    signal: TerminalSignal<'_>,
) -> bool {
    let TerminalSignal {
        prompt_id,
        stop_reason,
        agent_result,
        cancel_trigger,
        cancellation_category,
        cancellation_context,
    } = signal;
    if agent.session.loading_replay {
        return false;
    }
    if !(agent.session.state.is_turn_running() || agent.session.state.is_cancelling()) {
        return false;
    }
    let Some(current) = agent.session.current_prompt_id.clone() else {
        return false;
    };

    let (arm_pid, arm_via) = match prompt_id {
        Some(pid) if pid == current.as_str() => (current, "exact"),
        Some("") => {
            if driver_mid_active_work(agent) {
                return false;
            }
            (current, "empty_wire_pid")
        }
        Some(_) => return false,
        None => {
            if driver_mid_active_work(agent) {
                return false;
            }
            (current, "missing_wire_pid")
        }
    };

    if let Some(pending) = agent.pending_turn_end_reconcile.as_ref() {
        if pending.prompt_id != arm_pid {
            return false;
        }
        // Same pid already armed — keep earliest received_at; refresh outcome.
        let received_at = pending.received_at;
        agent.pending_turn_end_reconcile = Some(super::agent_view::PendingTurnEnd {
            prompt_id: arm_pid.clone(),
            stop_reason: stop_reason.map(str::to_string),
            agent_result: agent_result.map(str::to_string),
            cancel_trigger: cancel_trigger.map(str::to_string),
            cancellation_category: cancellation_category.map(str::to_string),
            cancellation_context: cancellation_context.cloned(),
            received_at,
        });
        crate::unified_log::info(
            "turn.end_reconcile.armed",
            Some(session_id),
            Some(serde_json::json!({
                "prompt_id": arm_pid,
                "wire_prompt_id": prompt_id,
                "arm_via": arm_via,
                "stop_reason": stop_reason,
                "refreshed": true,
            })),
        );
        return true;
    }

    crate::unified_log::info(
        "turn.end_reconcile.armed",
        Some(session_id),
        Some(serde_json::json!({
            "prompt_id": arm_pid,
            "wire_prompt_id": prompt_id,
            "arm_via": arm_via,
            "stop_reason": stop_reason,
        })),
    );
    agent.pending_turn_end_reconcile = Some(super::agent_view::PendingTurnEnd {
        prompt_id: arm_pid,
        stop_reason: stop_reason.map(str::to_string),
        agent_result: agent_result.map(str::to_string),
        cancel_trigger: cancel_trigger.map(str::to_string),
        cancellation_category: cancellation_category.map(str::to_string),
        cancellation_context: cancellation_context.cloned(),
        received_at: std::time::Instant::now(),
    });
    true
}

fn driver_mid_active_work(agent: &AgentView) -> bool {
    use crate::acp::tracker::TurnActivity;
    // A write whose delta stream died is positive evidence of a dead stream;
    // whatever shows through it (an open thinking block, an earlier tool)
    // must not block lost-response recovery.
    if agent.session.tracker.has_stale_tool_call_write() {
        return false;
    }
    match agent.session.tracker.activity() {
        Some(
            TurnActivity::ToolRunning { .. }
            | TurnActivity::Thinking
            | TurnActivity::AutoCompacting
            | TurnActivity::Retrying { .. }
            | TurnActivity::WritingToolCall(_),
        ) => true,
        Some(TurnActivity::Responding | TurnActivity::Waiting(_)) | None => false,
    }
}

/// Finalize a turn from a terminal signal, shared by the `prompt_complete`
/// broadcast and the durable `TurnCompleted` update so both behave identically.
///
/// DRIVER (`!attached_as_viewer`): the `PromptResponse` RPC owns the turn
/// lifecycle (it carries context this signal lacks: error classes, rewind
/// bookkeeping, adoption hand-off), so do NOT finish the turn here — that would
/// race/double-finish on every normal turn end (the signal is emitted BEFORE the
/// RPC response is written). But the RPC response can be LOST in transit (leader
/// response routing / reconnect races), and
/// it is the ONLY exit from `TurnRunning`/`TurnCancelling`. So when the signal
/// refers to the turn this client is driving (exact pid, or missing/empty pid
/// while not mid-tool), arm a deferred reconcile: if the RPC lands within the
/// grace window it disarms this (see `TaskResult::PromptResponse`); otherwise
/// the event loop finishes the turn from it (`reconcile_overdue_turn_ends`).
///
/// VIEWER (`attached_as_viewer`): a viewer adopts the driver's turn and never
/// receives its `PromptResponse`, so this is its only non-interactive exit from
/// `TurnRunning`. Finish the turn and push the "Turn completed/cancelled/failed"
/// marker mapped from [`TerminalSignal::stop_reason`]. Idempotent: a
/// duplicate/stale terminal for an already-finished turn pushes nothing and
/// returns [`TerminalApply::Ignored`].
pub(super) fn finalize_turn_from_terminal(
    agent: &mut AgentView,
    session_id: &str,
    signal: TerminalSignal<'_>,
) -> TerminalApply {
    let TerminalSignal {
        prompt_id,
        stop_reason,
        agent_result,
        cancel_trigger,
        cancellation_category,
        cancellation_context,
    } = signal;
    if !agent.attached_as_viewer {
        if arm_driver_turn_end_reconcile(agent, session_id, signal) {
            return TerminalApply::ReconcileArmed;
        }
        return TerminalApply::Ignored;
    }

    // Viewer: the driver's turn ended — exit TurnRunning. Only act when a turn
    // is actually in progress so a stray/duplicate signal is harmless (a
    // duplicate finds the turn already finished here and pushes no marker).
    if !agent.session.state.is_busy() && agent.session.current_prompt_id.is_none() {
        return TerminalApply::Ignored;
    }

    // Capture elapsed BEFORE `mark_turn_finished()` clears `turn_started_at`. The
    // anchor was back-dated from the authoritative `turnStartMs` on adoption, so
    // this reads the same wall-clock duration the driver shows. Missing clock
    // stays `None` (same as the live driver) so we render "Turn completed."
    // rather than "Worked for 0.0s".
    let elapsed_ms = duration_to_elapsed_ms(agent.turn_elapsed());
    // Read before `finish_turn()` clears it; keys the pending stop-hook stash.
    let ending_prompt_id = agent
        .session
        .current_prompt_id
        .clone()
        .or_else(|| prompt_id.map(str::to_string));

    // Before `finish_turn`: the blocked-prompt requeue reads
    // `in_flight_prompt`, which finish_turn clears.
    note_hook_blocked_turn(
        agent,
        prompt_id,
        cancellation_category,
        cancellation_context,
    );
    agent.session.finish_turn(&mut agent.scrollback);

    // Wire meta wins; else the client-side expectation (older-shell fallback).
    // Taken at every viewer finalize so it can't go stale.
    let expected_send_now = agent.expect_send_now_cancel.take();
    let send_now_cancel = match cancel_trigger {
        Some(trigger) => trigger == "send_now",
        None => expected_send_now.is_some(),
    };

    // A viewer never receives the driver's `PromptResponse` RPC — the source of
    // the driver's "Worked for X" marker. Surface the equivalent here.
    let event = terminal_marker(TerminalMarkerInput {
        stop: TurnStopReason::from(stop_reason),
        elapsed_ms,
        agent_result,
        send_now_cancel,
        cancellation_category,
        error_banner_present: super::dispatch::scrollback_has_recent_error_banner(
            &agent.scrollback,
        ),
    });
    push_turn_terminal_marker(agent, event, ending_prompt_id.as_deref());

    agent.mark_turn_finished(TurnEnd::Completed);

    TerminalApply::ViewerFinalized
}

/// Map a [`finalize_turn_from_terminal`] outcome to the redraw/tick bool that
/// BOTH terminal rails (`prompt_complete` and the live `TurnCompleted`) RETURN
/// DIRECTLY, applying the viewer-finalize side effect. Keeping this mapping in
/// one place is load-bearing: the live `TurnCompleted` arm must return this
/// instead of routing through `changed && is_active` (see below).
///
/// - `Ignored` -> `false`.
/// - `ReconcileArmed` -> `true` UNCONDITIONALLY (not gated on visibility). The
///   lost-RPC reconcile sweep rides the animation tick, and the event loop only
///   re-arms the tick when a batch reports a change. A background-tab driver
///   (`is_active == false`) that armed the reconcile must still report the change
///   or `reconcile_overdue_turn_ends` never fires and the turn strands on
///   "Waiting…" — the exact bug this rail fixes.
/// - `ViewerFinalized` -> `true` only when `is_active` (drop pending adoption).
pub(super) fn apply_terminal_outcome(
    outcome: TerminalApply,
    app: &mut AppView,
    agent_id: AgentId,
    is_active: bool,
) -> bool {
    match outcome {
        TerminalApply::Ignored => false,
        TerminalApply::ReconcileArmed => true,
        TerminalApply::ViewerFinalized => {
            if let Some(p) = app.pending_running_adoptions.remove(&agent_id)
                && let Some(agent) = app.agents.get_mut(&agent_id)
            {
                agent.discard_pending_adoption_updates(&p.prompt_id);
            }
            is_active
        }
    }
}

#[cfg(test)]
#[path = "turn_completion/tests.rs"]
mod tests;
