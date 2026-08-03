//! Debounced `idle_prompt` notification extension.

use std::rc::Rc;
use std::time::Duration;

use xai_agent_lifecycle::LocalExtensionRegistryBuilder;
use xai_agent_lifecycle::{LocalSessionLifecycleContributor, LocalTurnLifecycleContributor};
use xai_agent_lifecycle::{
    SessionIdleInput, TurnAbortInput, TurnDoneInput, TurnErrorInput, TurnStartInput,
};

use super::super::*;
use super::{NotificationEvent, NotificationEventSink};

/// Default `idle_prompt` debounce (60s of user inactivity).
const DEFAULT_IDLE_NOTIFICATION_DELAY: Duration = Duration::from_secs(60);

/// Debounce between the session settling idle and the `idle_prompt` notification, so it fires only on sustained inactivity.
/// `GROK_IDLE_NOTIFICATION_DELAY_MS` overrides it (used by E2E tests).
fn idle_notification_delay() -> Duration {
    resolve_idle_notification_delay(std::env::var("GROK_IDLE_NOTIFICATION_DELAY_MS").ok())
}

/// Split from [`idle_notification_delay`] so the env parsing is testable without touching the process env.
fn resolve_idle_notification_delay(raw: Option<String>) -> Duration {
    raw.and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_IDLE_NOTIFICATION_DELAY)
}

/// Fires the `idle_prompt` notification hook once the session stays idle for the delay. Synthetic turns (auto-wake, drain, cron) only defer an
/// earned ping: they cancel the timer like any turn start, and their own idle settle re-arms it.
/// Covered by the headless E2E via `GROK_IDLE_NOTIFICATION_DELAY_MS`.
struct IdlePromptExtension {
    notification_event_sink: Rc<dyn NotificationEventSink>,
    timer: TaskSlot<()>,
    /// Only a completed turn earns a ping; aborted and errored turns do not (matching the old
    /// end_turn-only arming).
    last_turn_completed: std::cell::Cell<bool>,
}

#[async_trait::async_trait(?Send)]
impl LocalTurnLifecycleContributor for IdlePromptExtension {
    async fn on_turn_start(&self, _input: &TurnStartInput) {
        self.timer.cancel();
    }

    async fn on_turn_done(&self, _input: &TurnDoneInput) {
        self.last_turn_completed.set(true);
    }

    async fn on_turn_abort(&self, _input: &TurnAbortInput) {
        self.last_turn_completed.set(false);
    }

    async fn on_turn_error(&self, _input: &TurnErrorInput<'_>) {
        self.last_turn_completed.set(false);
    }
}

#[async_trait::async_trait(?Send)]
impl LocalSessionLifecycleContributor for IdlePromptExtension {
    async fn on_session_idle(&self, _input: &SessionIdleInput) {
        if !self.last_turn_completed.get() {
            return;
        }
        let notification_event_sink = Rc::clone(&self.notification_event_sink);
        let delay = idle_notification_delay();
        let handle = tokio::task::spawn_local(async move {
            tokio::time::sleep(delay).await;
            notification_event_sink.emit(NotificationEvent {
                notification_type: "idle_prompt",
                message: Some("Turn complete".into()),
                title: None,
                level: Some("info".into()),
            });
        });
        self.timer.arm(handle);
    }
}

pub(super) fn install(
    builder: &mut LocalExtensionRegistryBuilder,
    notification_event_sink: Rc<dyn NotificationEventSink>,
) {
    let extension = Rc::new(IdlePromptExtension {
        notification_event_sink,
        timer: TaskSlot::new(),
        last_turn_completed: std::cell::Cell::new(false),
    });
    builder.turn_lifecycle_contributor(extension.clone());
    builder.session_lifecycle_contributor(extension);
}

#[cfg(test)]
mod idle_notification_delay_tests {
    use super::{DEFAULT_IDLE_NOTIFICATION_DELAY, resolve_idle_notification_delay};
    use std::time::Duration;

    /// Missing env var → 60s default.
    #[test]
    fn defaults_to_claude_code_threshold() {
        assert_eq!(
            resolve_idle_notification_delay(None),
            Duration::from_secs(60)
        );
        assert_eq!(
            resolve_idle_notification_delay(None),
            DEFAULT_IDLE_NOTIFICATION_DELAY
        );
    }

    /// Pins the public `GROK_IDLE_NOTIFICATION_DELAY_MS` contract: a valid override is interpreted as milliseconds (the E2E seam depends on this).
    #[test]
    fn env_override_parses_millis() {
        assert_eq!(
            resolve_idle_notification_delay(Some("250".into())),
            Duration::from_millis(250)
        );
    }

    /// A malformed override falls back to the default instead of panicking.
    #[test]
    fn invalid_override_falls_back_to_default() {
        assert_eq!(
            resolve_idle_notification_delay(Some("not-a-number".into())),
            DEFAULT_IDLE_NOTIFICATION_DELAY
        );
    }
}
