use std::sync::Arc;

use crate::auth::error::RefreshTokenFailedReason;
use crate::auth::manager::RefreshReason;

use super::ExternalCommandRunner;
use super::RefreshOutcome;
use super::TokenRefresher;

/// Refreshes by re-running the operator's external auth binary via the async
/// external-command runner. Returns data only; mutation lives in
/// `refresh_chain` (honors the [`TokenRefresher`] no-mutation contract).
pub(crate) struct ExternalBinaryRefresher {
    runner: Arc<dyn ExternalCommandRunner>,
    command: String,
}

impl ExternalBinaryRefresher {
    pub(crate) fn new(runner: Arc<dyn ExternalCommandRunner>, command: String) -> Self {
        Self { runner, command }
    }

    /// A failed or timed-out binary run is a single-strike `Other` permanent
    /// failure. `Other` is non-sticky, so `PERMANENT_FAILURE_TTL` lets a flaky
    /// or briefly slow binary self-heal without `/login`. The async runner
    /// bounds every run and group-kills the child on timeout, so there is no
    /// wedged-process case that would need a separate transient outcome.
    fn record_failure(&self, message: &str) -> RefreshOutcome {
        tracing::warn!(%message, "auth: external binary refresh failed permanently");
        // No token key in the binary flow; the caller scopes the verdict.
        RefreshOutcome::permanent(RefreshTokenFailedReason::Other, None)
    }
}

#[async_trait::async_trait]
impl TokenRefresher for ExternalBinaryRefresher {
    async fn refresh(&self, reason: RefreshReason) -> RefreshOutcome {
        tracing::debug!(?reason, "auth: external binary refresh starting");
        match self.runner.run_external_command(&self.command).await {
            Some(auth) => {
                crate::unified_log::info("auth: external binary refresh succeeded", None, None);
                RefreshOutcome::success(auth)
            }
            None => {
                crate::unified_log::warn(
                    "auth: external binary refresh returned no token",
                    None,
                    None,
                );
                self.record_failure("external binary returned no token")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::GrokAuth;

    /// Minimal runner whose external command returns a fixed result.
    struct FakeRunner {
        external_result: Option<GrokAuth>,
    }
    #[async_trait::async_trait]
    impl ExternalCommandRunner for FakeRunner {
        async fn run_external_command(&self, _command: &str) -> Option<GrokAuth> {
            self.external_result.clone()
        }
    }

    /// A failed binary run is a single-strike `Other` permanent failure that is
    /// NON-sticky: it must age out via the TTL, never lock an external-binary
    /// user out forever. (Flipping this to a sticky reason would be a silent
    /// lockout regression.)
    #[tokio::test]
    async fn external_binary_failure_is_single_strike_non_sticky_permanent() {
        let refresher = ExternalBinaryRefresher::new(
            Arc::new(FakeRunner {
                external_result: None,
            }),
            "auth-binary".into(),
        );
        match refresher.refresh(RefreshReason::ServerRejected).await {
            RefreshOutcome::PermanentFailure { error, .. } => {
                assert_eq!(error.reason, RefreshTokenFailedReason::Other);
                assert!(
                    !error.reason.is_sticky(),
                    "external-binary failure must age out, not strand the user forever",
                );
            }
            other => panic!("a failed binary run must be a permanent Other failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn external_binary_success_returns_fresh_token() {
        let token = GrokAuth {
            key: "ext-fresh".into(),
            ..GrokAuth::test_default()
        };
        let refresher = ExternalBinaryRefresher::new(
            Arc::new(FakeRunner {
                external_result: Some(token),
            }),
            "auth-binary".into(),
        );
        match refresher.refresh(RefreshReason::ServerRejected).await {
            RefreshOutcome::Success(auth) => assert_eq!(auth.key, "ext-fresh"),
            other => panic!("a successful binary run must return Success, got {other:?}"),
        }
    }
}
