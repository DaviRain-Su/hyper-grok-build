//! Turn-task data, spawn/completion plumbing, and guards for `SessionActor`.

use super::*;

pub(super) struct TurnSubagentScopeGuard {
    current_prompt_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    prompt_id: String,
}

impl TurnSubagentScopeGuard {
    pub(super) fn new(
        current_prompt_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
        prompt_id: String,
    ) -> Self {
        Self {
            current_prompt_id,
            prompt_id,
        }
    }
}

impl Drop for TurnSubagentScopeGuard {
    fn drop(&mut self) {
        let mut current_prompt_id = self
            .current_prompt_id
            .lock()
            .expect("current_prompt_id mutex poisoned");
        if current_prompt_id.as_deref() == Some(self.prompt_id.as_str()) {
            *current_prompt_id = None;
        }
    }
}

/// RAII guard that stores `false` into an `is_turn_active` flag on drop.
/// Guarantees the flag is cleared on all exit paths (early returns, errors, panics).
pub(super) struct TurnActiveGuard(Option<Arc<std::sync::atomic::AtomicBool>>);

impl TurnActiveGuard {
    pub(super) fn activate(flag: Option<&Arc<std::sync::atomic::AtomicBool>>) -> Self {
        if let Some(f) = flag {
            f.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Self(flag.cloned())
    }
}

impl Drop for TurnActiveGuard {
    fn drop(&mut self) {
        if let Some(flag) = &self.0 {
            flag.store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

pub(crate) struct TurnInputRequest {
    pub(crate) prompt_id: String,
    pub(crate) input_origin: InputOrigin,
    pub(crate) prompt_blocks: Vec<ContentBlock>,
    pub(crate) prompt_mode: PromptMode,
    pub(crate) trace_gcs_config: Option<crate::session::repo_changes::TraceExportConfig>,
    pub(crate) artifact_tracker: Option<crate::upload::manifest::ArtifactTracker>,
    pub(crate) client_identifier: Option<String>,
    pub(crate) screen_mode: Option<String>,
    pub(crate) verbatim: bool,
    pub(crate) send_now: bool,
    pub(crate) json_schema: Option<serde_json::Value>,
    pub(crate) persist_ack: Option<oneshot::Sender<()>>,
    pub(crate) parsed_prompt_tx: Option<oneshot::Sender<ParsedPromptInfo>>,
    pub(crate) traceparent: Option<String>,
}

/// Completion-channel payload: prompt id, turn result, elapsed ms from `started_at`.
/// `elapsed_ms` is `None` when no running task supplied a start time.
pub(crate) struct TurnCompletionMsg {
    pub(crate) prompt_id: String,
    pub(crate) result: PromptTurnResult,
    pub(crate) elapsed_ms: Option<u64>,
}

pub(crate) struct AgentTask {
    pub(crate) prompt_id: String,
    pub(crate) handle: tokio::task::AbortHandle,
    /// Monotonic start of this running turn; complete and cancel both read it.
    pub(crate) started_at: std::time::Instant,
}

/// Saturating `Instant` delta in ms. No panic on overflow.
pub(crate) fn elapsed_ms_saturating(start: std::time::Instant, now: std::time::Instant) -> u64 {
    u64::try_from(now.saturating_duration_since(start).as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod elapsed_ms_tests {
    use super::elapsed_ms_saturating;

    #[test]
    fn elapsed_ms_saturating_uses_injected_instants() {
        let start = std::time::Instant::now();
        let later = start + std::time::Duration::from_millis(42);
        assert_eq!(elapsed_ms_saturating(start, later), 42);
        assert_eq!(elapsed_ms_saturating(later, start), 0);
    }
}

impl AgentTask {
    #[cfg(test)]
    pub(crate) fn new(prompt_id: &str, handle: tokio::task::AbortHandle) -> Self {
        Self {
            prompt_id: prompt_id.to_string(),
            handle,
            started_at: std::time::Instant::now(),
        }
    }

    pub(super) fn new_prompt(
        session: Arc<SessionActor>,
        request: TurnInputRequest,
        completion_tx: mpsc::UnboundedSender<TurnCompletionMsg>,
    ) -> Self {
        let started_at = std::time::Instant::now();
        Self {
            prompt_id: request.prompt_id.clone(),
            handle: tokio::task::spawn_local(run_task(session, request, completion_tx, started_at))
                .abort_handle(),
            started_at,
        }
    }

    pub(super) fn abort(&self) {
        if !self.handle.is_finished() {
            self.handle.abort();
        }
    }
}

/// Holds at most one spawned task; arming a new one aborts the previous.
/// `Cell` interior mutability because `SessionActor` is `!Send` (single-threaded
/// LocalSet). Backs both the deferred user-message prefix (`take`s the handle to
/// await its result) and the idle-notification debounce (`cancel`s it).
pub(crate) struct TaskSlot<T> {
    handle: std::cell::Cell<Option<tokio::task::JoinHandle<T>>>,
}

impl<T> TaskSlot<T> {
    pub(crate) fn new() -> Self {
        Self {
            handle: std::cell::Cell::new(None),
        }
    }

    /// Abort any pending task and store the new one.
    pub(crate) fn arm(&self, handle: tokio::task::JoinHandle<T>) {
        if let Some(old) = self.handle.take() {
            old.abort();
        }
        self.handle.set(Some(handle));
    }

    /// Take the pending task handle (e.g. to await its result).
    pub(crate) fn take(&self) -> Option<tokio::task::JoinHandle<T>> {
        self.handle.take()
    }

    /// Abort and drop any pending task (e.g. because a new turn started).
    pub(crate) fn cancel(&self) {
        if let Some(old) = self.handle.take() {
            old.abort();
        }
    }
}

async fn run_task(
    session: Arc<SessionActor>,
    request: TurnInputRequest,
    completion_tx: mpsc::UnboundedSender<TurnCompletionMsg>,
    started_at: std::time::Instant,
) {
    let prompt_id = request.prompt_id.clone();
    let result = session.handle_turn_input(request).await;
    let elapsed_ms = elapsed_ms_saturating(started_at, std::time::Instant::now());
    let _ = completion_tx.send(TurnCompletionMsg {
        prompt_id,
        result,
        elapsed_ms: Some(elapsed_ms),
    });
}

#[cfg(test)]
mod task_slot_tests {
    // Exercises the shared `TaskSlot<T>` primitive that backs both the deferred
    // prefix and the idle-notification debounce: arm / take / cancel / re-arm.
    use super::TaskSlot;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Spawn a 60s task that bumps `fired` by `by`, armed into `slot`.
    fn arm_counter(slot: &TaskSlot<()>, fired: &Arc<AtomicUsize>, by: usize) {
        let f = Arc::clone(fired);
        slot.arm(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            f.fetch_add(by, Ordering::SeqCst);
        }));
    }

    /// A task left armed runs to completion once its delay elapses.
    #[tokio::test(start_paused = true)]
    async fn armed_task_fires_after_delay() {
        let fired = Arc::new(AtomicUsize::new(0));
        let slot: TaskSlot<()> = TaskSlot::new();
        arm_counter(&slot, &fired, 1);

        let task = slot.take().expect("task armed");
        tokio::time::advance(Duration::from_secs(61)).await;
        let _ = task.await;

        assert_eq!(fired.load(Ordering::SeqCst), 1, "armed task must fire");
    }

    /// `cancel()` aborts a still-pending task (the new-user-prompt reset path).
    #[tokio::test(start_paused = true)]
    async fn cancel_aborts_pending_task() {
        let fired = Arc::new(AtomicUsize::new(0));
        let slot: TaskSlot<()> = TaskSlot::new();
        arm_counter(&slot, &fired, 1);

        tokio::time::advance(Duration::from_secs(30)).await;
        slot.cancel();
        assert!(slot.take().is_none(), "cancel must clear the slot");
        tokio::time::advance(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;

        assert_eq!(
            fired.load(Ordering::SeqCst),
            0,
            "cancelled task must not fire"
        );
    }

    /// Arming a new task aborts the previous one so only the latest fires.
    #[tokio::test(start_paused = true)]
    async fn rearm_aborts_previous_task() {
        let fired = Arc::new(AtomicUsize::new(0));
        let slot: TaskSlot<()> = TaskSlot::new();
        arm_counter(&slot, &fired, 1);
        arm_counter(&slot, &fired, 10);

        let task = slot.take().expect("task armed");
        tokio::time::advance(Duration::from_secs(61)).await;
        let _ = task.await;
        tokio::task::yield_now().await;

        assert_eq!(
            fired.load(Ordering::SeqCst),
            10,
            "only the re-armed task fires"
        );
    }
}
