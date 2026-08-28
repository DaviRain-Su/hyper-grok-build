//! Shell-owned control handle for one active child session.

use tokio::sync::{mpsc, oneshot};
use xai_grok_tools::implementations::grok_build::task::coordinator::{
    ActiveMessageAdmission, ChildControl, LocalBoxFuture, SendBoxFuture, SubagentProgress,
};
use xai_grok_tools::implementations::grok_build::task::types::ActiveAgentMessageDelivery;

use super::prompt_turn_receipt::{PromptTurnReceipt, cancel_shell_child_turn};
use crate::session::{SessionCommand, SessionThread};

/// Shell runtime handle retained while a child is active.
pub(crate) struct ShellChildRuntime {
    pub(crate) child_cmd_tx: mpsc::UnboundedSender<SessionCommand>,
    pub(crate) child_signals: crate::session::signals::SessionSignalsHandle,
    /// Held by the worker until promotion succeeds; `None` while the caller
    /// still owns the join handle so cancel-at-promote can wait for exit.
    pub(crate) _child_thread: Option<SessionThread>,
    pub(crate) receipt_sink: mpsc::Sender<PromptTurnReceipt>,
    pub(crate) active_message_parent_session_id: String,
    pub(crate) active_message_parent_prompt_index: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl ChildControl for ShellChildRuntime {
    type ProgressFuture = LocalBoxFuture<SubagentProgress>;

    fn progress(&self) -> Self::ProgressFuture {
        let signals = self.child_signals.clone();
        Box::pin(async move {
            signals
                .snapshot()
                .await
                .map(|snapshot| SubagentProgress {
                    turn_count: snapshot.turn_count,
                    tool_call_count: snapshot.tool_call_count,
                    tokens_used: snapshot.context_tokens_used,
                    context_window_tokens: snapshot.context_window_tokens,
                    context_usage_pct: snapshot.context_window_usage,
                    tools_used: snapshot.tools_used,
                    error_count: snapshot.error_count,
                })
                .unwrap_or_default()
        })
    }

    fn send_active_message(
        &self,
        delivery: ActiveAgentMessageDelivery,
    ) -> SendBoxFuture<ActiveMessageAdmission> {
        let cmd_tx = self.child_cmd_tx.clone();
        let receipt_sink = self.receipt_sink.clone();
        let parent_prompt_index = self
            .active_message_parent_prompt_index
            .load(std::sync::atomic::Ordering::Acquire);
        let parent_telemetry_ctx = xai_grok_telemetry::TelemetryCtx::new(
            self.active_message_parent_session_id.clone(),
            std::sync::Arc::new(tokio::sync::Mutex::new(parent_prompt_index)),
        );
        Box::pin(async move {
            let (respond_to, response_rx) = oneshot::channel();
            if cmd_tx
                .send(SessionCommand::ParentAgentMessage {
                    delivery,
                    receipt_sink,
                    parent_telemetry_ctx,
                    respond_to,
                })
                .is_err()
            {
                return ActiveMessageAdmission::ChannelClosed;
            }
            response_rx
                .await
                .unwrap_or(ActiveMessageAdmission::ChannelClosed)
        })
    }

    fn cancel(&self) {
        cancel_shell_child_turn(&self.child_cmd_tx);
    }
}

const SESSION_THREAD_EXIT_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// Bound for waiting out an unpromoted child actor after cancel+shutdown.
pub(crate) const UNPROMOTED_SESSION_THREAD_EXIT_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5);

/// Poll `SessionThread::is_finished` until the actor thread exits or `timeout`.
/// `Duration::ZERO` is a single observation and never sleeps.
pub(crate) async fn await_session_thread_exit(
    thread: &SessionThread,
    timeout: std::time::Duration,
) -> bool {
    if thread.is_finished() {
        return true;
    }
    if timeout.is_zero() {
        return false;
    }
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return thread.is_finished();
        }
        tokio::time::sleep(remaining.min(SESSION_THREAD_EXIT_POLL)).await;
        if thread.is_finished() {
            return true;
        }
    }
}

/// Whether an unpromoted child may drop its worktree and workspace binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnpromotedResourceFate {
    Release,
    Preserve,
}

impl UnpromotedResourceFate {
    pub(crate) fn from_thread_exit(exited: bool) -> Self {
        if exited {
            Self::Release
        } else {
            Self::Preserve
        }
    }

    pub(crate) fn should_release(self) -> bool {
        matches!(self, Self::Release)
    }
}

#[cfg(test)]
#[path = "child_runtime_tests.rs"]
mod tests;
