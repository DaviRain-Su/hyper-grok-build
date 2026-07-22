#![cfg_attr(rustfmt, rustfmt::skip)]
#![allow(unused_imports)]
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use agent_client_protocol as acp;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use crate::extensions::notification::{SessionNotification, SessionUpdate};
use crate::session::{
    self, SessionCommand, SessionHandle, SessionThread,
    commands::{PromptCompletionKind, PromptTurnResult as SubagentPromptTurnResult},
    fs_watch::FsWatchCapabilities, info::Info as SessionInfo,
};
use crate::terminal::AsyncTerminalRunner;
use crate::tools::ToolContext;
use crate::upload::trace::{
    GCS_SCHEMA_VERSION, PromptMetadata, SubagentSpawnedRef, TurnResultMetadata,
    local_sandbox_telemetry, upload_metadata, upload_session_state,
    upload_subagent_metadata, upload_turn_result,
};
use crate::upload::turn::{PromptTraceContext, complete_prompt_trace};
use xai_acp_lib::AcpAgentGatewaySender as GatewaySender;
use xai_grok_tools::implementations::grok_build::task::types::*;
use xai_grok_workspace::file_system::AsyncFileSystem;
use xai_hunk_tracker::HunkTrackerHandle;
use super::*;
impl SubagentCoordinator {
    /// Synchronous lookup of a subagent by ID.
    ///
    /// Returns a three-way result so the caller can drop the `RefCell` borrow
    /// before awaiting the signals handle for running subagents.
    ///
    /// - `Ready` — completed/failed/cancelled snapshot, no async work needed.
    /// - `NeedsSignals` — subagent is running; caller must await
    ///   `resolve_snapshot()` after dropping the coordinator borrow.
    /// - `None` — ID not found in active, completed, or pending maps.
    pub(crate) fn lookup(&self, id: &str) -> Option<SnapshotLookup> {
        if let Some(tracker) = self.active.get(id) {
            if tracker.owner.is_workflow() {
                return None;
            }
            return Some(
                SnapshotLookup::NeedsSignals(RunningSnapshotSeed {
                    subagent_id: tracker.subagent_id.clone(),
                    description: tracker.description.clone(),
                    subagent_type: tracker.subagent_type.clone(),
                    started_at_epoch_ms: instant_to_epoch_ms(tracker.started_at),
                    duration_ms: tracker.started_at.elapsed().as_millis() as u64,
                    persona: tracker.persona.clone(),
                    signals_handle: tracker.child_handle.signals_handle.clone(),
                }),
            );
        }
        if let Some(completed) = self.completed.get(id) {
            if completed.owner.is_workflow() {
                return None;
            }
            let status = if completed.result.cancelled {
                SubagentSnapshotStatus::Cancelled {
                    reason: completed.result.error.clone(),
                }
            } else if completed.result.success {
                let output = match &completed.persisted_output_dir {
                    Some(dir) => {
                        read_subagent_output(dir)
                            .unwrap_or_else(|| {
                                OUTPUT_UNAVAILABLE_PLACEHOLDER.to_string()
                            })
                    }
                    None => completed.result.output.to_string(),
                };
                SubagentSnapshotStatus::Completed {
                    output,
                    tool_calls: completed.result.tool_calls,
                    turns: completed.result.turns,
                    worktree_path: completed.result.worktree_path.clone(),
                }
            } else {
                SubagentSnapshotStatus::Failed {
                        error: completed
                            .result
                            .error
                            .clone()
                            .unwrap_or_else(|| "Unknown error".to_string()),
                    }
            };
            return Some(
                SnapshotLookup::Ready(SubagentSnapshot {
                    subagent_id: completed.subagent_id.clone(),
                    description: completed.description.clone(),
                    subagent_type: completed.subagent_type.clone(),
                    status,
                    started_at_epoch_ms: instant_to_epoch_ms(completed.started_at),
                    duration_ms: completed.result.duration_ms,
                    persona: completed.persona.clone(),
                }),
            );
        }
        if let Some(pending) = self.pending.get(id) {
            if pending.owner.is_workflow() {
                return None;
            }
            return Some(
                SnapshotLookup::Ready(SubagentSnapshot {
                    subagent_id: pending.subagent_id.clone(),
                    description: pending.description.clone(),
                    subagent_type: pending.subagent_type.clone(),
                    status: SubagentSnapshotStatus::Initializing,
                    started_at_epoch_ms: instant_to_epoch_ms(pending.started_at),
                    duration_ms: pending.started_at.elapsed().as_millis() as u64,
                    persona: pending.persona.clone(),
                }),
            );
        }
        None
    }
    /// Parent session of the running subagent whose child session is
    /// `child_session_id`. Used to re-parent spawn requests that originate
    /// inside a child session (e.g. a loop iteration spawning its own
    /// subagent) to the root session that owns it.
    pub(crate) fn parent_of_child_session(
        &self,
        child_session_id: &str,
    ) -> Option<String> {
        self.active
            .values()
            .find(|t| t.child_session_id.0.as_ref() == child_session_id)
            .map(|t| t.parent_session_id.clone())
    }
    /// Return `(parent_session_id, child_session_id)` for a given subagent.
    ///
    /// Checks active first, then completed. Returns `None` if not found.
    pub(crate) fn session_ids_for(&self, id: &str) -> Option<(String, String)> {
        if let Some(t) = self.active.get(id) {
            return Some((t.parent_session_id.clone(), t.child_session_id.0.to_string()));
        }
        if let Some(c) = self.completed.get(id) {
            return Some((c.parent_session_id.clone(), c.child_session_id.clone()));
        }
        None
    }
    /// Mark a subagent as block-waited so auto-wake is suppressed on completion.
    pub(crate) fn mark_block_waited(&mut self, id: &str) {
        if let Some(t) = self.active.get_mut(id) {
            t.block_waited = true;
        } else if let Some(c) = self.completed.get_mut(id) {
            c.block_waited = true;
        }
    }
    /// Clear the block-waited flag after a block timed out without receiving
    /// the completion, so auto-wake can still fire when the subagent finishes.
    pub(crate) fn clear_block_waited(&mut self, id: &str) {
        if let Some(t) = self.active.get_mut(id) {
            t.block_waited = false;
        } else if let Some(c) = self.completed.get_mut(id) {
            c.block_waited = false;
        }
    }
    /// Whether a block-waiter already consumed this subagent's result.
    pub(crate) fn is_block_waited(&self, id: &str) -> bool {
        self.active.get(id).is_some_and(|t| t.block_waited)
            || self.completed.get(id).is_some_and(|c| c.block_waited)
    }
    /// Register a live blocking-query reply slot and mark `block_waited`.
    ///
    /// The slot lets `block_wait_delivered_or_live` verify at completion
    /// time that the waiter can still receive the result — the flag alone
    /// can be stale when the waiting turn was cancelled moments before the
    /// subagent finished.
    pub(crate) fn register_block_wait(&mut self, id: &str, slot: BlockWaitSlot) {
        self.mark_block_waited(id);
        self.block_wait_slots.entry(id.to_string()).or_default().push(slot);
    }
    /// Drop a previously registered reply slot (query poll loop exited).
    pub(crate) fn unregister_block_wait(&mut self, id: &str, slot: &BlockWaitSlot) {
        if let Some(slots) = self.block_wait_slots.get_mut(id) {
            slots.retain(|s| !std::rc::Rc::ptr_eq(s, slot));
            if slots.is_empty() {
                self.block_wait_slots.remove(id);
            }
        }
    }
    /// Decision-time gate for the completion auto-wake: returns true when
    /// the result was already delivered to a blocking waiter, or a live
    /// waiter is still parked and will receive it. When every registered
    /// waiter is gone (receivers dropped by a cancelled turn), clears
    /// `block_waited` and returns false so the auto-wake fires.
    ///
    /// This closes the race where the query poll loop clears the flag up to
    /// one poll interval *after* the caller cancelled — the completion
    /// handler could read the stale flag in that window and skip the wake.
    /// Consumes the id's slot registrations (completion is terminal).
    pub(crate) fn block_wait_delivered_or_live(&mut self, id: &str) -> bool {
        let slots = self.block_wait_slots.remove(id).unwrap_or_default();
        if !self.is_block_waited(id) {
            return false;
        }
        let delivered_or_live = slots.is_empty()
            || slots
                .iter()
                .any(|s| s.borrow().as_ref().is_none_or(|tx| !tx.is_closed()));
        if !delivered_or_live {
            self.clear_block_waited(id);
        }
        delivered_or_live
    }
    /// Mark a subagent as explicitly killed so auto-wake is suppressed on completion.
    pub(crate) fn mark_explicitly_killed(&mut self, id: &str) {
        if let Some(t) = self.active.get_mut(id) {
            t.explicitly_killed = true;
        } else if let Some(c) = self.completed.get_mut(id) {
            c.explicitly_killed = true;
        }
    }
    /// Whether the model explicitly killed this subagent via the kill tool.
    pub(crate) fn is_explicitly_killed(&self, id: &str) -> bool {
        self.active.get(id).is_some_and(|t| t.explicitly_killed)
            || self.completed.get(id).is_some_and(|c| c.explicitly_killed)
    }
    /// Return fork provenance for a given subagent.
    pub(crate) fn provenance_for(&self, id: &str) -> SubagentProvenance {
        if let Some(t) = self.active.get(id) {
            return SubagentProvenance {
                fork_parent_prompt_id: t.parent_prompt_id.clone(),
                resumed_from: t.resumed_from.clone(),
            };
        }
        if let Some(c) = self.completed.get(id) {
            return SubagentProvenance {
                fork_parent_prompt_id: c.parent_prompt_id.clone(),
                resumed_from: c.resumed_from.clone(),
            };
        }
        SubagentProvenance::default()
    }
    /// Resolve a completed subagent scoped to the requesting parent session.
    ///
    /// Returns `None` if the subagent is not found, still active, or belongs
    /// to a different parent session (prevents cross-session context bleed).
    ///
    /// Fast path: checks the in-memory `completed` map first. When that
    /// misses (e.g. after cap eviction), falls back to on-disk metadata
    /// in `{parent_session_dir}/subagents/{id}/meta.json`.
    pub(crate) fn resumable_source_for(
        &self,
        id: &str,
        parent_session_id: &str,
        parent_cwd: &Path,
    ) -> Option<ResumeSourceData> {
        if let Some(completed) = self.completed.get(id) {
            if completed.parent_session_id != parent_session_id {
                return None;
            }
            return Some(ResumeSourceData {
                subagent_id: completed.subagent_id.clone(),
                child_session_id: completed.child_session_id.clone(),
                child_cwd: completed.child_cwd.clone(),
                worktree_path: completed.worktree_path.clone(),
                snapshot_ref: completed.snapshot_ref.clone(),
                subagent_type: completed.subagent_type.clone(),
                persona: completed.persona.clone(),
                model_id: Some(completed.effective_model_id.clone()),
            });
        }
        let parent_info = SessionInfo {
            id: acp::SessionId::new(parent_session_id),
            cwd: parent_cwd.to_string_lossy().to_string(),
        };
        let meta_path = session::persistence::session_dir(&parent_info)
            .join("subagents")
            .join(id)
            .join("meta.json");
        let data = std::fs::read_to_string(&meta_path).ok()?;
        let meta: SubagentMeta = serde_json::from_str(&data).ok()?;
        if meta.parent_session_id != parent_session_id {
            return None;
        }
        match meta.status.as_str() {
            "completed" | "failed" | "cancelled" => {}
            _ => return None,
        }
        Some(ResumeSourceData {
            subagent_id: meta.subagent_id,
            child_session_id: meta.child_session_id,
            child_cwd: meta.child_cwd.unwrap_or_default(),
            worktree_path: meta.worktree_path.map(PathBuf::from),
            snapshot_ref: meta.snapshot_ref,
            subagent_type: meta.subagent_type,
            persona: meta.persona,
            model_id: meta.effective_model_id,
        })
    }
    /// Check whether an ID refers to a currently-active (running) subagent.
    pub(crate) fn is_active(&self, id: &str) -> bool {
        self.active.contains_key(id)
    }
    /// Whether the coordinator still has this id in flight (spawning or running).
    /// Orphan reconcile skips these — there is nothing stuck to heal.
    pub(crate) fn is_active_or_pending(&self, id: &str) -> bool {
        self.active.contains_key(id) || self.pending.contains_key(id)
    }
    pub(crate) fn record_loop_owner(&mut self, subagent_id: &str, task_id: &str) {
        self.loop_owned.insert(subagent_id.to_string(), task_id.to_string());
    }
    pub(crate) fn remove_loop_owner(&mut self, subagent_id: &str) {
        self.loop_owned.remove(subagent_id);
    }
    pub(crate) fn loop_task_id_of_child_session(
        &self,
        child_session_id: &str,
    ) -> Option<String> {
        let subagent_id = self
            .active
            .values()
            .find(|t| t.child_session_id.0.as_ref() == child_session_id)?
            .subagent_id
            .clone();
        self.loop_owned.get(&subagent_id).cloned()
    }
    pub(crate) fn loop_unit_active(&self, task_id: &str) -> bool {
        self.loop_owned.values().any(|t| t == task_id)
    }
    /// The terminal `SubagentFinished` for an id the coordinator already holds in
    /// `completed`, else `None`. Lets orphan reconcile re-emit a subagent's real
    /// outcome when only its terminal meta write was lost (reconnect race: entry
    /// in `completed` but the on-disk meta is still `running`) instead of
    /// force-cancelling it and discarding the result.
    pub(crate) fn completed_finish(&self, id: &str) -> Option<SessionUpdate> {
        let c = self.completed.get(id)?;
        let duration_ms = c
            .completed_at
            .saturating_duration_since(c.started_at)
            .as_millis() as u64;
        Some(SessionUpdate::SubagentFinished {
            subagent_id: c.subagent_id.clone(),
            child_session_id: c.child_session_id.clone(),
            status: c.result.status().to_string(),
            error: c.result.error.clone(),
            tool_calls: c.result.tool_calls,
            turns: c.result.turns,
            duration_ms,
            tokens_used: 0,
            output: None,
            will_wake: false,
        })
    }
    /// Lifecycle-map entry counts as `(pending, active, completed)`.
    pub(crate) fn registry_snapshot(&self) -> (usize, usize, usize) {
        (self.pending.len(), self.active.len(), self.completed.len())
    }
    /// Oldest completions are evicted first; their `output.json` stays on disk.
    pub fn enforce_completed_cap(&mut self) {
        if self.completed.len() <= MAX_COMPLETED_ENTRIES {
            return;
        }
        let excess = self.completed.len() - MAX_COMPLETED_ENTRIES;
        let mut by_age: Vec<(std::time::Instant, String)> = self
            .completed
            .iter()
            .map(|(id, e)| (e.completed_at, id.clone()))
            .collect();
        by_age.sort_unstable_by_key(|(completed_at, _)| *completed_at);
        for (_, id) in by_age.into_iter().take(excess) {
            self.completed.remove(&id);
        }
    }
    /// Snapshot all currently-running subagents for compaction state context.
    ///
    /// Completed/failed/cancelled subagents are NOT included — they live in
    /// the `completed` map and are irrelevant for post-compaction reminders
    /// (the model already saw their tool results before compaction).
    ///
    /// The `elapsed_ms` field is computed from `started_at.elapsed()` at call
    /// time, so the values are a snapshot of "right now" — appropriate for
    /// compaction since it happens once and the reminder is static.
    #[cfg(test)]
    pub fn active_summaries(&self) -> Vec<ActiveSubagentSummary> {
        self.active
            .values()
            .filter(|t| !t.owner.is_workflow())
            .map(tracker_to_summary)
            .collect()
    }
    pub fn active_summaries_for(
        &self,
        parent_session_id: &str,
    ) -> Vec<ActiveSubagentSummary> {
        self.active
            .values()
            .filter(|t| {
                t.parent_session_id == parent_session_id && !t.owner.is_workflow()
            })
            .map(tracker_to_summary)
            .collect()
    }
    ///
    /// Each seed carries copied identity metadata plus a cloned
    /// `SessionSignalsHandle` so the caller can resolve live progress
    /// asynchronously after dropping the coordinator borrow.
    ///
    /// Returns an empty `Vec` if no active subagents match the given
    /// parent session ID. Callers (e.g. the `x.ai/subagent/list_running`
    /// ACP handler) should treat an empty result as a normal "no running
    /// subagents" response, not an error.
    pub(crate) fn list_running_for_parent(
        &self,
        parent_session_id: &str,
    ) -> Vec<RunningSubagentListSeed> {
        self.active
            .values()
            .filter(|t| {
                t.parent_session_id == parent_session_id && !t.owner.is_workflow()
            })
            .map(|t| RunningSubagentListSeed {
                subagent_id: t.subagent_id.clone(),
                parent_session_id: t.parent_session_id.clone(),
                child_session_id: t.child_session_id.0.to_string(),
                subagent_type: t.subagent_type.clone(),
                description: t.description.clone(),
                started_at_epoch_ms: instant_to_epoch_ms(t.started_at),
                duration_ms: t.started_at.elapsed().as_millis() as u64,
                signals_handle: t.child_handle.signals_handle.clone(),
            })
            .collect()
    }
}
