//! Pins `handle_sampling_failure`'s transient arm: eligible kinds retry
//! while budget remains; exhaustion and the kill switch fall through;
//! dedicated-recovery kinds are untouched.

use super::support::*;
use super::*;
use std::sync::Arc;
use tokio::sync::mpsc;

fn error_of_kind(
    kind: xai_grok_sampler::SamplingErrorKind,
    status_code: Option<u16>,
) -> xai_grok_sampler::SamplingErrorInfo {
    xai_grok_sampler::SamplingErrorInfo {
        kind,
        message: "test sampling failure".to_string(),
        status_code,
        is_retryable: false,
        retry_after_secs: None,
        should_retry: None,
        error_code: None,
        model_metadata: None,
        empty_response_context: None,
        doom_loop_triggers: None,
        doom_loop_aborted_at_chunk: None,
        credential: xai_grok_sampling_types::SentCredential::Unknown,
    }
}

async fn make_actor() -> (Arc<SessionActor>, mpsc::UnboundedReceiver<PersistenceMsg>) {
    let (gateway_tx, _) = mpsc::unbounded_channel();
    let (persistence_tx, persistence_rx) = mpsc::unbounded_channel();
    let actor = create_test_actor(50_000, 100_000, 85, gateway_tx, persistence_tx).await;
    (Arc::new(actor), persistence_rx)
}

/// The eligibility classifier is a closed set: transient infrastructure
/// kinds retry, everything with its own recovery/UX stays terminal.
#[test]
fn transient_retry_eligibility_truth_table() {
    use xai_grok_sampler::SamplingErrorKind as K;

    assert!(transient_retry_eligible(&error_of_kind(
        K::IdleTimeout,
        None
    )));
    assert!(transient_retry_eligible(&error_of_kind(K::Http, None)));
    for status in [500, 502, 503, 504, 520, 530] {
        assert!(
            transient_retry_eligible(&error_of_kind(K::Api, Some(status))),
            "5xx Api ({status}) must be eligible"
        );
    }

    // Parity with `is_retryable_api_status`: origin-TLS 52x, client errors
    // (incl. 408/429), and status-less Api all fail closed.
    for status in [
        Some(525),
        Some(526),
        Some(400),
        Some(404),
        Some(408),
        Some(422),
        Some(429),
        None,
    ] {
        assert!(
            !transient_retry_eligible(&error_of_kind(K::Api, status)),
            "Api ({status:?}) must stay terminal"
        );
    }

    for kind in [
        K::Auth,
        K::Serialization,
        K::RateLimited,
        K::EmptyResponse,
        K::MaxTokensTruncation,
        K::DoomLoopDetected,
    ] {
        assert!(
            !transient_retry_eligible(&error_of_kind(kind, None)),
            "{kind:?} must not enter the transient retry arm"
        );
    }
}

/// Vetoes mirror `is_retry_vetoed`: `x-should-retry: false` and
/// context-window overflow, whatever status wrapped them.
#[test]
fn retry_vetoes_mirror_sampler_classifier() {
    use xai_grok_sampler::SamplingErrorKind as K;

    for kind in [K::IdleTimeout, K::Http] {
        let mut error = error_of_kind(kind, None);
        error.should_retry = Some(false);
        assert!(
            !transient_retry_eligible(&error),
            "{kind:?} with should_retry=false must stay terminal"
        );
    }
    let mut vetoed = error_of_kind(K::Api, Some(503));
    vetoed.should_retry = Some(false);
    assert!(!transient_retry_eligible(&vetoed));

    let mut overflow = error_of_kind(K::Api, Some(500));
    overflow.message =
        "This model's maximum context length is 262144 tokens, please reduce".to_string();
    assert!(
        !transient_retry_eligible(&overflow),
        "5xx-wrapped context overflow must stay terminal"
    );
}

/// Backoff ladder indexed by attempts used, clamped at the last rung.
#[test]
fn backoff_ladder_clamps_at_last_rung() {
    use std::time::Duration;
    assert_eq!(transient_backoff_delay(0), Duration::from_secs(2));
    assert_eq!(transient_backoff_delay(1), Duration::from_secs(10));
    assert_eq!(transient_backoff_delay(2), Duration::from_secs(30));
    assert_eq!(transient_backoff_delay(9), Duration::from_secs(30));
    // Raising a bound is a conscious decision: repin these literals with it.
    assert_eq!(MAX_TRANSIENT_TURN_RETRIES, 3);
    assert_eq!(MAX_TRANSIENT_RETRIES_PER_PROMPT, 10);
    assert_eq!(MAX_TRANSIENT_RETRY_WINDOW, Duration::from_secs(600));
}

/// First idle-timeout failure of a step: back off and resubmit rather than
/// killing the turn.
#[tokio::test(flavor = "current_thread")]
async fn idle_timeout_first_failure_requests_resubmit() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = make_actor().await;
            let result = actor
                .handle_sampling_failure(
                    error_of_kind(xai_grok_sampler::SamplingErrorKind::IdleTimeout, None),
                    0,
                    transient_state(0, true),
                )
                .await;
            match result {
                Ok(SamplerFailureRecovery::RetryTransient { kind, .. }) => {
                    assert_eq!(kind, xai_grok_sampler::SamplingErrorKind::IdleTimeout);
                }
                Ok(_) => panic!("expected RetryTransient, got another recovery"),
                Err(e) => panic!("first idle timeout must not be terminal: {e:?}"),
            }
        })
        .await;
}

/// A 503 that exhausted the sampler's internal retries gets the same
/// turn-level resubmit.
#[tokio::test(flavor = "current_thread")]
async fn server_error_first_failure_requests_resubmit() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = make_actor().await;
            let result = actor
                .handle_sampling_failure(
                    error_of_kind(xai_grok_sampler::SamplingErrorKind::Api, Some(503)),
                    0,
                    transient_state(0, true),
                )
                .await;
            match result {
                Ok(SamplerFailureRecovery::RetryTransient { status_code, .. }) => {
                    assert_eq!(status_code, Some(503));
                }
                Ok(_) => panic!("expected RetryTransient, got another recovery"),
                Err(e) => panic!("first 503 must not be terminal: {e:?}"),
            }
        })
        .await;
}

/// A spent budget surfaces exactly as before the retry arm existed.
#[tokio::test(flavor = "current_thread")]
async fn exhausted_budget_falls_through_to_terminal() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = make_actor().await;
            let result = actor
                .handle_sampling_failure(
                    error_of_kind(xai_grok_sampler::SamplingErrorKind::IdleTimeout, None),
                    0,
                    transient_state(MAX_TRANSIENT_TURN_RETRIES, true),
                )
                .await;
            assert!(
                result.is_err(),
                "an idle timeout past the retry budget must surface as terminal"
            );
        })
        .await;
}

/// The kill switch disables the arm even with full budget.
#[tokio::test(flavor = "current_thread")]
async fn kill_switch_disables_the_arm() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = make_actor().await;
            let result = actor
                .handle_sampling_failure(
                    error_of_kind(xai_grok_sampler::SamplingErrorKind::IdleTimeout, None),
                    0,
                    transient_state(0, false),
                )
                .await;
            assert!(
                result.is_err(),
                "with the kill switch off, an eligible failure must stay terminal"
            );
        })
        .await;
}

/// Budgeted workflow children never enter the arm: the fail-closed usage
/// accounting guards precede it and stay terminal.
#[tokio::test(flavor = "current_thread")]
async fn budgeted_workflow_child_stays_terminal() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = mpsc::unbounded_channel();
            let (persistence_tx, _rx) = mpsc::unbounded_channel();
            let mut actor =
                create_test_actor(50_000, 100_000, 85, gateway_tx, persistence_tx).await;
            actor.tool_context.task_output_token_budget = Some(
                crate::tools::tool_context::TaskOutputTokenBudget::limited(1_000),
            );
            let actor = Arc::new(actor);
            let result = actor
                .handle_sampling_failure(
                    error_of_kind(xai_grok_sampler::SamplingErrorKind::Api, Some(503)),
                    0,
                    transient_state(0, true),
                )
                .await;
            assert!(
                result.is_err(),
                "an eligible 503 in a budgeted child must stay terminal"
            );
        })
        .await;
}

/// Empty responses stay terminal regardless of budget (the reasoning-only
/// doom-loop invariant also pinned by the replay-buffer tests).
#[tokio::test(flavor = "current_thread")]
async fn empty_response_stays_terminal_with_full_budget() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = make_actor().await;
            let result = actor
                .handle_sampling_failure(
                    error_of_kind(xai_grok_sampler::SamplingErrorKind::EmptyResponse, None),
                    0,
                    transient_state(0, true),
                )
                .await;
            assert!(
                result.is_err(),
                "empty response must stay terminal, not enter the transient retry arm"
            );
        })
        .await;
}

/// The cumulative per-prompt cap holds even with a fresh per-step budget:
/// long agentic prompts must not multiply retries by round count.
#[tokio::test(flavor = "current_thread")]
async fn prompt_total_cap_vetoes_even_with_fresh_step_budget() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = make_actor().await;
            let state = TransientRetryState {
                prompt_attempts: MAX_TRANSIENT_RETRIES_PER_PROMPT,
                ..transient_state(0, true)
            };
            let result = actor
                .handle_sampling_failure(
                    error_of_kind(xai_grok_sampler::SamplingErrorKind::Api, Some(503)),
                    0,
                    state,
                )
                .await;
            assert!(
                result.is_err(),
                "a spent prompt budget must stay terminal even with step budget left"
            );
        })
        .await;
}

/// A recovery episode older than the wall-clock window is terminal: idle
/// stalls burn a detector cycle per attempt, so count alone cannot bound time.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn episode_window_vetoes_after_wall_clock_budget() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = make_actor().await;
            // Paused clock: advance past the window instead of back-dating a
            // live monotonic clock (which underflows on a freshly booted host).
            let episode_start = Some(tokio::time::Instant::now());
            tokio::time::advance(MAX_TRANSIENT_RETRY_WINDOW + std::time::Duration::from_secs(1))
                .await;
            let state = TransientRetryState {
                episode_start,
                ..transient_state(0, true)
            };
            let result = actor
                .handle_sampling_failure(
                    error_of_kind(xai_grok_sampler::SamplingErrorKind::IdleTimeout, None),
                    0,
                    state,
                )
                .await;
            assert!(
                result.is_err(),
                "an episode past the wall-clock window must stay terminal"
            );
        })
        .await;
}

/// Deterministic image rejections are excluded however the proxy wrapped
/// them: the sampler's own classifier already stripped images and gave up.
#[tokio::test(flavor = "current_thread")]
async fn invalid_image_code_is_never_transient_retried() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = make_actor().await;
            let mut error = error_of_kind(xai_grok_sampler::SamplingErrorKind::Api, Some(500));
            error.error_code = Some(xai_grok_sampling_types::ApiErrorCode::InvalidImage);
            let result = actor
                .handle_sampling_failure(error, 0, transient_state(0, true))
                .await;
            assert!(
                result.is_err(),
                "an invalid_image 500 must stay terminal, not be resent three more times"
            );
        })
        .await;
}
