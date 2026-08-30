//! Mid-turn wait interrupt: remember aborted wait ids (union across
//! concurrent aborts) and strip extras from the next wait that is a
//! proper superset. Apply never forgets the set (siblings in the same
//! batch still see it). Complete drops only the finished ids.

use xai_grok_tools::types::output::{ToolOutput as ToolsToolOutput, ToolRunResult};
use xai_tool_types::{TaskOutputOutput, TaskOutputResult};

use crate::tools::tool_context::BlockingWaitState;

/// Outcome of applying the remembered interrupted wait to a new wait call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum InterruptedWaitFilter {
    Rewritten { kept: Vec<String>, requested: usize },
    Unchanged,
}

/// `task_ids` wins over singular `task_id`; same lenient list + trim/dedup
/// as the wait tool, so a payload it would run cannot yield an empty set here.
pub(super) fn wait_task_ids_from_args(args: &serde_json::Value) -> Vec<String> {
    let raw = args
        .get("task_ids")
        .or_else(|| args.get("task_id"))
        .and_then(xai_tool_types::serde_lenient::lenient_string_list_from_json)
        .unwrap_or_default();
    xai_tool_types::resolve_task_ids(&raw)
}

/// Extras drop only when the next interruptible wait contains every
/// interrupted id plus at least one new one.
fn wait_ids_after_interrupt(interrupted: &[String], requested: &[String]) -> Option<Vec<String>> {
    if interrupted.is_empty() || requested.is_empty() {
        return None;
    }
    let extra = requested.iter().any(|id| !interrupted.contains(id));
    let missing = interrupted.iter().any(|id| !requested.contains(id));
    if extra && !missing {
        Some(interrupted.to_vec())
    } else {
        None
    }
}

fn write_rewritten_task_ids(parsed_args: &mut serde_json::Value, kept: &[String]) {
    if let Some(obj) = parsed_args.as_object_mut() {
        obj.insert("task_ids".to_string(), serde_json::json!(kept));
        // `task_id` is a serde alias of `task_ids`; leaving both keys
        // makes TaskOutputToolInput deserialize fail.
        obj.remove("task_id");
    }
}

/// Apply never forgets the set: a sibling wait in the same dispatch
/// batch still needs it. Empty / non-superset waits leave it unchanged.
pub(super) fn apply_interrupted_wait_filter(
    state: &BlockingWaitState,
    parsed_args: &mut serde_json::Value,
) -> InterruptedWaitFilter {
    let requested = wait_task_ids_from_args(parsed_args);
    if requested.is_empty() {
        return InterruptedWaitFilter::Unchanged;
    }
    state
        .update_interrupted_wait(state.generation(), |remembered| {
            let Some(interrupted) = remembered.as_ref() else {
                return InterruptedWaitFilter::Unchanged;
            };
            match wait_ids_after_interrupt(interrupted, &requested) {
                Some(kept) => {
                    write_rewritten_task_ids(parsed_args, &kept);
                    InterruptedWaitFilter::Rewritten {
                        requested: requested.len(),
                        kept,
                    }
                }
                None => InterruptedWaitFilter::Unchanged,
            }
        })
        .unwrap_or(InterruptedWaitFilter::Unchanged)
}

fn is_terminal_wait_status(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "cancelled" | "timed_out" | "not_found"
    )
}

/// Ids that actually finished. A `timeout_ms` wait leaves running tasks
/// non-terminal, so those ids must stay in the remembered set.
/// `not_found` / `TaskNotFound` are gone: keep them and later supersets
/// keep stripping new work against dead ids.
pub(super) fn finished_wait_ids(result: &ToolRunResult) -> Vec<String> {
    match &result.output {
        ToolsToolOutput::TaskOutput(TaskOutputOutput::Result(res))
            if is_terminal_wait_status(&res.status) && !res.task_id.is_empty() =>
        {
            vec![res.task_id.clone()]
        }
        ToolsToolOutput::TaskOutput(TaskOutputOutput::TaskNotFound(id)) if !id.is_empty() => {
            vec![id.clone()]
        }
        ToolsToolOutput::TaskOutput(TaskOutputOutput::MultiResult(multi)) => multi
            .results
            .iter()
            .filter(|res| is_terminal_wait_status(&res.status) && !res.task_id.is_empty())
            .map(|res| res.task_id.clone())
            .collect(),
        _ => Vec::new(),
    }
}

/// Abort unions. Complete drops only terminal ids so a timeout or
/// concurrent abort does not wipe still-running work.
pub(super) fn record_interruptible_wait_outcome(
    state: &BlockingWaitState,
    generation: u64,
    waited_ids: Vec<String>,
    aborted: bool,
    finished_ids: &[String],
) {
    if waited_ids.is_empty() && finished_ids.is_empty() {
        return;
    }
    let _ = state.update_interrupted_wait(generation, |remembered| {
        if aborted {
            match remembered {
                Some(existing) => {
                    for id in waited_ids {
                        if !existing.contains(&id) {
                            existing.push(id);
                        }
                    }
                }
                None => *remembered = Some(waited_ids),
            }
        } else if let Some(existing) = remembered {
            existing.retain(|id| !finished_ids.contains(id));
            if existing.is_empty() {
                *remembered = None;
            }
        }
    });
}

pub(super) const WAIT_INTERRUPTED_HEAD: &str = "Wait interrupted: the user sent a message.";

fn interrupted_wait_prompt(has_ids: bool) -> String {
    if !has_ids {
        return WAIT_INTERRUPTED_HEAD.to_string();
    }
    format!(
        "{WAIT_INTERRUPTED_HEAD}\n\n\
         If you choose to wait on these tasks again, resume with the called task_ids only. \
         Newly spawned background work auto-wakes when it finishes and doesn't need to be added to this wait."
    )
}

/// Model-facing result when a wait is aborted for a pending interjection.
pub(super) fn interrupted_wait_tool_result(args: &serde_json::Value) -> ToolRunResult {
    let ids = wait_task_ids_from_args(args);
    let msg = interrupted_wait_prompt(!ids.is_empty());
    let task_id = ids.first().cloned().unwrap_or_default();
    let result = TaskOutputResult {
        task_id,
        command: String::new(),
        status: "cancelled".to_string(),
        exit_code: None,
        started: String::new(),
        ended: None,
        duration_secs: 0.0,
        output: msg.clone(),
        output_file: String::new(),
        truncated: false,
        truncation_hint: String::new(),
        raw_output_bytes: msg.len(),
    };
    ToolRunResult {
        output: ToolsToolOutput::TaskOutput(TaskOutputOutput::Result(result)),
        prompt_text: msg,
        effective_tool_name: None,
    }
}

#[cfg(test)]
#[path = "wait_interrupt_tests.rs"]
mod wait_interrupt_tests;
