use super::support::*;
use super::*;
use crate::auth::{AuthManager, AuthMode, GrokAuth, GrokComConfig};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

/// Test refresher that returns a fresh token and records that it
/// was invoked. Used to drive the auth-arm success path.
struct AlwaysSucceedRefresher {
    called: Arc<AtomicBool>,
}
#[async_trait::async_trait]
impl crate::auth::refresh::TokenRefresher for AlwaysSucceedRefresher {
    async fn refresh(
        &self,
        _reason: crate::auth::refresh::RefreshReason,
    ) -> crate::auth::refresh::RefreshOutcome {
        self.called.store(true, Ordering::SeqCst);
        crate::auth::refresh::RefreshOutcome::Success(Box::new(GrokAuth {
            key: "refreshed-test-token".to_string(),
            auth_mode: AuthMode::Oidc,
            refresh_token: Some("rt-new".into()),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            ..GrokAuth::test_default()
        }))
    }
}

/// `(tempdir, manager)` with an expired OIDC token loaded so
/// `unauthorized_recovery()` actually dispatches to the refresher.
/// Tempdir must outlive the manager (auth.json path).
///
/// Use the env-independent test constructor: temporarily unsetting
/// `GROK_AUTH` / `GROK_AUTH_PATH` here would clobber another parallel test's
/// fixture and could make it read the developer's real credential file.
fn auth_manager_with_refresher(
    refresher: Arc<dyn crate::auth::refresh::TokenRefresher>,
) -> (tempfile::TempDir, Arc<AuthManager>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let am = Arc::new(AuthManager::new_test_isolated(
        dir.path(),
        GrokComConfig::default(),
    ));
    am.hot_swap(GrokAuth {
        key: "initial-test-key".into(),
        auth_mode: AuthMode::Oidc,
        refresh_token: Some("rt".into()),
        expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
        ..GrokAuth::test_default()
    });
    am.set_refresher(refresher);
    (dir, am)
}

/// Build a `SamplingErrorInfo` of kind Auth - the same shape the
/// inner `OaiCompatClient` emit surfaces after recording its own
/// attribution.
fn auth_error() -> xai_grok_sampler::SamplingErrorInfo {
    xai_grok_sampler::SamplingErrorInfo {
        kind: xai_grok_sampler::SamplingErrorKind::Auth,
        message: "Unauthorized (401)".to_string(),
        status_code: Some(401),
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

/// Construct a test actor with the supplied `auth_manager` and
/// session-token credentials wired in. Wraps the actor in `Arc`
/// ready for `handle_sampling_failure`.
async fn make_actor_with_auth_manager(
    auth_manager: Option<Arc<AuthManager>>,
) -> (Arc<SessionActor>, mpsc::UnboundedReceiver<PersistenceMsg>) {
    make_actor_with_auth_and_credentials(
        auth_manager,
        xai_chat_state::AuthType::SessionToken,
        "initial-test-key".to_string(),
    )
    .await
}

/// Variant that pins the credential `auth_type`; the `auth_method_id` is
/// derived from it. Use [`make_actor_with_method_and_credentials`] to pin the
/// two independently.
async fn make_actor_with_auth_and_credentials(
    auth_manager: Option<Arc<AuthManager>>,
    auth_type: xai_chat_state::AuthType,
    api_key: String,
) -> (Arc<SessionActor>, mpsc::UnboundedReceiver<PersistenceMsg>) {
    let method_id = match auth_type {
        xai_chat_state::AuthType::SessionToken => "cached_token",
        xai_chat_state::AuthType::ApiKey => "xai.api_key",
    };
    make_actor_with_method_and_credentials(auth_manager, method_id, auth_type, api_key).await
}

/// Pin the ACP `auth_method_id` and credential `auth_type` independently. The
/// gate keys off the stable `auth_method_id`, so this reproduces the regression:
/// a session method whose `creds.auth_type` has transiently collapsed to
/// `ApiKey` (session-token cache miss + `XAI_API_KEY`).
async fn make_actor_with_method_and_credentials(
    auth_manager: Option<Arc<AuthManager>>,
    auth_method_id: &str,
    auth_type: xai_chat_state::AuthType,
    api_key: String,
) -> (Arc<SessionActor>, mpsc::UnboundedReceiver<PersistenceMsg>) {
    let (gateway_tx, _) = mpsc::unbounded_channel();
    let (persistence_tx, persistence_rx) = mpsc::unbounded_channel();
    let mut actor = create_test_actor(50_000, 100_000, 85, gateway_tx, persistence_tx).await;
    actor.auth_manager = auth_manager;
    actor.auth_method_id = test_auth_method_id(auth_method_id);
    actor
        .chat_state_handle
        .update_credentials(xai_chat_state::Credentials {
            api_key: Some(api_key),
            auth_type,
            ..Default::default()
        });
    (Arc::new(actor), persistence_rx)
}

/// `(tempdir, manager)` holding a valid OIDC token (so `get_valid_token()` is a
/// cache hit). The tempdir must outlive the manager (auth.json path).
///
/// See [`auth_manager_with_refresher`] for why this bypasses process-global
/// auth environment variables.
fn auth_manager_with_valid_token(key: &str) -> (tempfile::TempDir, Arc<AuthManager>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let am = Arc::new(AuthManager::new_test_isolated(
        dir.path(),
        GrokComConfig::default(),
    ));
    am.hot_swap(GrokAuth {
        key: key.into(),
        auth_mode: AuthMode::Oidc,
        refresh_token: Some("rt".into()),
        expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
        ..GrokAuth::test_default()
    });
    (dir, am)
}

/// Sub-case 1: no auth_manager -> falls through, no emit.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn no_emit_when_auth_manager_is_none() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = make_actor_with_auth_manager(None).await;
            crate::auth::attribution::reset_test_emit_count();
            let _ = actor.handle_sampling_failure(auth_error()).await;
            assert_eq!(
                crate::auth::attribution::test_emit_count(),
                0,
                "auth arm must not emit attribution when no auth_manager is wired"
            );
        })
        .await;
}

/// Sub-case 2: no AuthManager → auth recovery is skipped entirely,
/// falls through to terminal error. Covers BYOK / API-key users
/// where no OIDC refresh is possible.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn no_recovery_without_auth_manager() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = make_actor_with_auth_and_credentials(
                None,
                xai_chat_state::AuthType::ApiKey,
                "xai-byok-key".to_string(),
            )
            .await;
            crate::auth::attribution::reset_test_emit_count();
            let result = actor.handle_sampling_failure(auth_error()).await;
            assert!(
                result.is_err(),
                "no auth manager must fall through to terminal error"
            );
            assert_eq!(
                crate::auth::attribution::test_emit_count(),
                0,
                "auth arm must not emit attribution without auth manager"
            );
        })
        .await;
}

/// Session-based auth + working refresher → RefreshAuthAndResubmit.
#[tokio::test(flavor = "current_thread")]
async fn sampler_401_recovery_returns_refresh_and_retry() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_auth_manager(Some(am)).await;
            let result = actor.handle_sampling_failure(auth_error()).await;
            assert!(
                matches!(
                    result,
                    Ok(SamplerFailureRecovery::RefreshAuthAndResubmit {
                        store: RecoveredStore::SessionToken,
                        ..
                    })
                ),
                "session-based auth with a working refresher must return RefreshAuthAndResubmit"
            );
            assert!(called.load(Ordering::SeqCst), "refresher must be invoked");
        })
        .await;
}

/// Regression: sampler 401 with API-key auth (BYOK `env_key` /
/// `XAI_API_KEY`) must NOT attempt an OIDC session-token refresh. The
/// bearer on the wire is the static API key, so refreshing the session
/// token reports success but the retry re-sends the same rejected key —
/// an invisible 401 loop that hangs the turn. Recovery is skipped and
/// the 401 surfaces as a terminal error.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn sampler_401_with_api_key_auth_skips_refresh_and_surfaces_error() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_auth_and_credentials(
                Some(am),
                xai_chat_state::AuthType::ApiKey,
                "xai-byok-key".to_string(),
            )
            .await;

            let result = actor.handle_sampling_failure(auth_error()).await;

            assert!(
                result.is_err(),
                "API-key 401 must surface a terminal error, not retry"
            );
            assert!(
                !called.load(Ordering::SeqCst),
                "API-key 401 must NOT trigger an OIDC session-token refresh"
            );
        })
        .await;
}

/// Per-turn pre-flight refresh must not fire when `creds.auth_type` is
/// `ApiKey` (a BYOK model): the model's own API key must not be overwritten
/// by the session JWT.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn pre_flight_refresh_skips_api_key_auth_type() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_auth_and_credentials(
                Some(am),
                xai_chat_state::AuthType::ApiKey,
                "byok-api-key".to_string(),
            )
            .await;
            actor.refresh_token_if_expired().await;
            assert!(
                !called.load(Ordering::SeqCst),
                "pre-flight refresh must NOT fire for ApiKey auth_type"
            );
            assert_eq!(
                actor
                    .chat_state_handle
                    .get_credentials()
                    .await
                    .api_key
                    .as_deref(),
                Some("byok-api-key"),
                "BYOK api_key must not be overwritten by session token refresh"
            );
        })
        .await;
}

/// Hard-expired session token: pre-flight must call the refresher and must
/// not leave credentials stuck while pretending the JWT/config path applies.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn pre_flight_refreshes_hard_expired_session_token() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            assert!(
                !am.has_usable_token(),
                "precondition: access token is hard-expired"
            );

            let (actor, _rx) = make_actor_with_auth_manager(Some(am.clone())).await;
            actor.refresh_token_if_expired().await;

            assert!(
                called.load(Ordering::SeqCst),
                "pre-flight must invoke the refresher for a hard-expired session token"
            );
            assert_eq!(
                actor
                    .chat_state_handle
                    .get_credentials()
                    .await
                    .api_key
                    .as_deref(),
                Some("refreshed-test-token"),
                "credentials must be updated to the refreshed bearer"
            );
            assert!(am.has_usable_token());
        })
        .await;
}

/// Hard-expired + failed refresh: do not fall through to JWT/config.toml;
/// strip the chat-state seed so default headers cannot carry a dead AT.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn pre_flight_hard_expired_refresh_failure_skips_jwt_fallthrough() {
    // Unset BYOK platform API-key env vars (ANTHROPIC_API_KEY, OPENAI_API_KEY,
    // …): `model_auth_facts` consults the live catalog, whose built-in
    // platform entries carry `env_key` for the platform's API-key name. A
    // developer shell with one of these set flips the model's BYOK status to
    // `Byok`, deactivating the `SessionTokenAuthGate` and steering
    // `refresh_token_if_expired` away from `get_valid_token` (the path that
    // invokes the refresher this test asserts against). Without this guard
    // the pre-flight never calls the refresher and `call_count` stays at 0.
    //
    // `#[serial]` (default group) so this env mutation is ordered against the
    // cli_models / enterprise_byok / mvp_agent / config tests that set
    // GROK_AUTH / GROK_AUTH_PATH and would otherwise race this test's reads.
    let _byok = xai_grok_test_support::unset_all_byok_platform_api_key_envs();
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let call_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> = Arc::new({
                struct AlwaysFail(Arc<std::sync::atomic::AtomicU32>);
                #[async_trait::async_trait]
                impl crate::auth::refresh::TokenRefresher for AlwaysFail {
                    async fn refresh(
                        &self,
                        _: crate::auth::refresh::RefreshReason,
                    ) -> crate::auth::refresh::RefreshOutcome {
                        self.0.fetch_add(1, Ordering::SeqCst);
                        crate::auth::refresh::RefreshOutcome::transient("refresh failed")
                    }
                }
                AlwaysFail(call_count.clone())
            });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_auth_manager(Some(am.clone())).await;

            actor.refresh_token_if_expired().await;

            assert!(
                call_count.load(Ordering::SeqCst) >= 1,
                "pre-flight must attempt refresh"
            );
            assert_eq!(
                actor
                    .chat_state_handle
                    .get_credentials()
                    .await
                    .api_key
                    .as_deref(),
                None,
                "hard-expired pre-flight failure must strip the chat-state seed"
            );
            assert!(
                !am.has_usable_token(),
                "token remains hard-expired after failed refresh"
            );
            assert!(
                am.permanent_failure().is_none(),
                "transient refresh failure must not poison permanent_failure"
            );
        })
        .await;
}

/// Regression: per-turn pre-flight refresh must not fire for a third-party
/// BYOK platform endpoint even when the BYOK gate misclassifies the model as
/// `NotByok` (live-only catalog entries are invisible to the offline lookup).
/// Otherwise the stamped platform key in chat-state credentials is replaced
/// by the xAI session token before the sampler even builds its config.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn pre_flight_refresh_skips_open_platform_endpoint() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            // Session-based method (cached_token) — the shape that activates
            // the session gate for a `NotByok` classification.
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::ApiKey,
                "ollama-platform-key".to_string(),
            )
            .await;
            if let Some(mut cfg) = actor.chat_state_handle.get_sampling_config().await {
                cfg.base_url = "https://ollama.com/v1".into();
                cfg.model = "ollama-live-only-model-not-in-offline-catalog".into();
                actor.chat_state_handle.update_sampling_config(cfg);
            }

            actor.refresh_token_if_expired().await;

            assert!(
                !called.load(Ordering::SeqCst),
                "pre-flight session refresh must NOT fire for an open-platform endpoint"
            );
            assert_eq!(
                actor
                    .chat_state_handle
                    .get_credentials()
                    .await
                    .api_key
                    .as_deref(),
                Some("ollama-platform-key"),
                "stamped platform key must not be overwritten by the session token"
            );
        })
        .await;
}

/// Soft-expired (early-invalidation buffer) + transient fail: retain the seed
/// so a still-accepted wire AT can continue until 401 recovery.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn pre_flight_soft_expired_transient_fail_retains_seed() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let call_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> = Arc::new({
                struct AlwaysFail(Arc<std::sync::atomic::AtomicU32>);
                #[async_trait::async_trait]
                impl crate::auth::refresh::TokenRefresher for AlwaysFail {
                    async fn refresh(
                        &self,
                        _: crate::auth::refresh::RefreshReason,
                    ) -> crate::auth::refresh::RefreshOutcome {
                        self.0.fetch_add(1, Ordering::SeqCst);
                        crate::auth::refresh::RefreshOutcome::transient("refresh failed")
                    }
                }
                AlwaysFail(call_count.clone())
            });
            let dir = tempfile::tempdir().expect("tempdir");
            let am = Arc::new(AuthManager::new_test_isolated(
                dir.path(),
                GrokComConfig::default(),
            ));
            // Inside the early-invalidation buffer but still hard-valid.
            am.hot_swap(GrokAuth {
                key: "buffered-test-key".into(),
                auth_mode: AuthMode::Oidc,
                refresh_token: Some("rt".into()),
                expires_at: Some(chrono::Utc::now() + chrono::Duration::seconds(30)),
                ..GrokAuth::test_default()
            });
            am.set_refresher(refresher);
            let (actor, _rx) = make_actor_with_auth_and_credentials(
                Some(am.clone()),
                xai_chat_state::AuthType::SessionToken,
                "buffered-test-key".to_string(),
            )
            .await;

            actor.refresh_token_if_expired().await;

            assert!(
                call_count.load(Ordering::SeqCst) >= 1,
                "soft-expired pre-flight must still attempt refresh"
            );
            assert_eq!(
                actor
                    .chat_state_handle
                    .get_credentials()
                    .await
                    .api_key
                    .as_deref(),
                Some("buffered-test-key"),
                "buffer-window soft-expired + transient fail must retain seed"
            );
            assert!(
                am.has_usable_token(),
                "token inside hard-expiry buffer remains usable"
            );
        })
        .await;
}

/// Proactive refresh keeps the cache hot so `refresh_token_if_expired`
/// (per-turn pre-flight) is a cache hit — the refresher fires once
/// (proactive), then the per-turn call sees the fresh token without
/// hitting the IdP again.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn proactive_refresh_makes_per_turn_refresh_a_cache_hit() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let call_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> = Arc::new({
                struct Counting(Arc<std::sync::atomic::AtomicU32>);
                #[async_trait::async_trait]
                impl crate::auth::refresh::TokenRefresher for Counting {
                    async fn refresh(
                        &self,
                        _: crate::auth::refresh::RefreshReason,
                    ) -> crate::auth::refresh::RefreshOutcome {
                        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        crate::auth::refresh::RefreshOutcome::Success(Box::new(GrokAuth {
                            key: "proactive-fresh".into(),
                            auth_mode: AuthMode::Oidc,
                            refresh_token: Some("rt-new".into()),
                            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                            ..GrokAuth::test_default()
                        }))
                    }
                }
                Counting(call_count.clone())
            });

            let (_dir, am) = auth_manager_with_refresher(refresher);
            let cancel = tokio_util::sync::CancellationToken::new();
            am.start_proactive_refresh(cancel.clone());

            // Wait for the proactive task to fire; its first pass runs after
            // PROACTIVE_MIN_SLEEP, so the window must exceed the floor.
            tokio::time::sleep(
                crate::auth::manager::PROACTIVE_MIN_SLEEP + std::time::Duration::from_millis(1000),
            )
            .await;
            assert!(
                call_count.load(Ordering::SeqCst) >= 1,
                "proactive task must have fired"
            );
            let count_after_proactive = call_count.load(Ordering::SeqCst);

            // Now run refresh_token_if_expired (the per-turn pre-flight).
            // It should see the proactively-refreshed token and NOT invoke
            // the refresher again.
            let (actor, _rx) = make_actor_with_auth_manager(Some(am)).await;
            actor.refresh_token_if_expired().await;

            assert_eq!(
                call_count.load(Ordering::SeqCst),
                count_after_proactive,
                "per-turn refresh must NOT call the refresher again (cache hit)"
            );
            assert_eq!(
                actor
                    .chat_state_handle
                    .get_credentials()
                    .await
                    .api_key
                    .as_deref(),
                Some("proactive-fresh"),
                "per-turn refresh must pick up the proactively-refreshed token"
            );

            cancel.cancel();
        })
        .await;
}

fn model_not_found_error() -> xai_grok_sampler::SamplingErrorInfo {
    xai_grok_sampler::SamplingErrorInfo {
            kind: xai_grok_sampler::SamplingErrorKind::Api,
            message: "API error (status 404 Not Found): The model grok-build does not exist or your team does not have access".into(),
            status_code: Some(404),
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

/// 404 model-not-found with a legacy WebLogin token appends a
/// "Legacy auth detected" hint to the error message.
#[tokio::test(flavor = "current_thread")]
async fn legacy_auth_hint_on_404_model_not_found() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let am = Arc::new(AuthManager::new_test_isolated(
                dir.path(),
                GrokComConfig::default(),
            ));
            am.hot_swap(GrokAuth {
                key: "legacy-token".into(),
                auth_mode: AuthMode::WebLogin,
                ..GrokAuth::test_default()
            });

            let (actor, _rx) = make_actor_with_auth_manager(Some(am)).await;
            let result = actor.handle_sampling_failure(model_not_found_error()).await;
            let err = match result {
                Err(e) => e,
                Ok(_) => panic!("expected Err from handle_sampling_failure"),
            };
            let data = err.data.unwrap();
            let msg = data.as_str().unwrap();
            assert!(
                msg.contains("deprecated authentication method"),
                "404 with WebLogin must include deprecation message, got: {msg}"
            );
            assert!(
                msg.contains("grok update"),
                "hint must mention `grok update` before re-login, got: {msg}"
            );
            assert!(
                msg.contains("grok logout"),
                "hint must mention `grok logout`, got: {msg}"
            );
            assert!(
                msg.contains("grok login"),
                "hint must mention `grok login`, got: {msg}"
            );
            let update_at = msg.find("grok update").expect("grok update");
            let logout_at = msg.find("grok logout").expect("grok logout");
            assert!(
                update_at < logout_at,
                "update must come before logout, got: {msg}"
            );
            assert!(
                msg.contains("Version:"),
                "must show client version, got: {msg}"
            );
        })
        .await;
}

/// Build a 401-shaped error that bypasses step 4b's auth recovery.
///
/// In production, 401s arrive as `SamplingErrorKind::Auth` with
/// `status_code: None`. Step 4b intercepts `Auth`-kind errors and
/// runs the full recovery chain — which succeeds on devbox/CI
/// environments via SA-token mint, masking the hint.
///
/// Using `Api` kind + `status_code: Some(401)` exercises the hint
/// condition (`status_code == Some(401)`) without triggering
/// recovery, making the test environment-independent.
fn unauthorized_401_error() -> xai_grok_sampler::SamplingErrorInfo {
    xai_grok_sampler::SamplingErrorInfo {
            kind: xai_grok_sampler::SamplingErrorKind::Api,
            message: "Unauthorized (401) from https://cli-chat-proxy.grok.com/v1/responses: {\"error\":\"Invalid or expired credentials (auth_kind=bearer, x_xai_token_auth=xai-grok-cli, upstream=Unauthenticated, reason=no auth context)\"}".into(),
            status_code: Some(401),
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

/// 401 Unauthorized with a legacy WebLogin token appends a
/// "Legacy auth detected" hint to the error message.
#[tokio::test(flavor = "current_thread")]
async fn legacy_auth_hint_on_401_unauthorized() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let am = Arc::new(AuthManager::new_test_isolated(
                dir.path(),
                GrokComConfig::default(),
            ));
            am.hot_swap(GrokAuth {
                key: "legacy-token".into(),
                auth_mode: AuthMode::WebLogin,
                ..GrokAuth::test_default()
            });

            let (actor, _rx) = make_actor_with_auth_manager(Some(am)).await;
            let result = actor
                .handle_sampling_failure(unauthorized_401_error())
                .await;
            let err = match result {
                Err(e) => e,
                Ok(_) => panic!("expected Err from handle_sampling_failure"),
            };
            let data = err.data.unwrap();
            let msg = data.as_str().unwrap();
            assert!(
                msg.contains("deprecated authentication method"),
                "401 with WebLogin must include deprecation message, got: {msg}"
            );
            assert!(
                msg.contains("grok update"),
                "hint must mention `grok update` before re-login, got: {msg}"
            );
            assert!(
                msg.contains("grok logout"),
                "hint must mention `grok logout`, got: {msg}"
            );
            assert!(
                msg.contains("grok login"),
                "hint must mention `grok login`, got: {msg}"
            );
            let update_at = msg.find("grok update").expect("grok update");
            let logout_at = msg.find("grok logout").expect("grok logout");
            assert!(
                update_at < logout_at,
                "update must come before logout, got: {msg}"
            );
        })
        .await;
}

/// 401 with OIDC auth must NOT append the legacy hint.
#[tokio::test(flavor = "current_thread")]
async fn no_legacy_hint_on_401_for_oidc_auth() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let am = Arc::new(AuthManager::new_test_isolated(
                dir.path(),
                GrokComConfig::default(),
            ));
            am.hot_swap(GrokAuth {
                key: "oidc-token".into(),
                auth_mode: AuthMode::Oidc,
                refresh_token: Some("rt".into()),
                expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                ..GrokAuth::test_default()
            });

            let (actor, _rx) = make_actor_with_auth_manager(Some(am)).await;
            let result = actor
                .handle_sampling_failure(unauthorized_401_error())
                .await;
            let err = match result {
                Err(e) => e,
                Ok(_) => panic!("expected Err from handle_sampling_failure"),
            };
            let data = err.data.unwrap();
            let msg = data
                .get("message")
                .and_then(|v| v.as_str())
                .or_else(|| data.as_str())
                .unwrap();
            assert!(
                !msg.contains("deprecated authentication method"),
                "OIDC auth must NOT trigger WebLogin deprecation on 401, got: {msg}"
            );
            assert!(
                msg.contains("Auth:      Oidc"),
                "OIDC 401 must show auth mode in enriched message, got: {msg}"
            );
        })
        .await;
}

/// 404 model-not-found with OIDC auth must NOT append the legacy hint.
#[tokio::test(flavor = "current_thread")]
async fn no_legacy_hint_for_oidc_auth() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let am = Arc::new(AuthManager::new_test_isolated(
                dir.path(),
                GrokComConfig::default(),
            ));
            am.hot_swap(GrokAuth {
                key: "oidc-token".into(),
                auth_mode: AuthMode::Oidc,
                refresh_token: Some("rt".into()),
                expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                ..GrokAuth::test_default()
            });

            let (actor, _rx) = make_actor_with_auth_manager(Some(am)).await;
            let result = actor.handle_sampling_failure(model_not_found_error()).await;
            let err = match result {
                Err(e) => e,
                Ok(_) => panic!("expected Err from handle_sampling_failure"),
            };
            let data = err.data.unwrap();
            let msg = data
                .get("message")
                .and_then(|v| v.as_str())
                .or_else(|| data.as_str())
                .unwrap();
            assert!(
                !msg.contains("deprecated authentication method"),
                "OIDC auth must NOT trigger WebLogin deprecation, got: {msg}"
            );
            assert!(
                msg.contains("Auth:      Oidc"),
                "OIDC 404 must show auth mode in enriched message, got: {msg}"
            );
            assert!(
                msg.contains("Version:"),
                "OIDC 404 must show version in enriched message, got: {msg}"
            );
        })
        .await;
}

// Regression group: a live session whose `auth_type` transiently reads `ApiKey`
// must still recover, because the gate keys off the stable `auth_method_id`.
#[test]
fn session_token_auth_gate_truth_table() {
    use crate::agent::auth_method::{ModelByok, session_token_auth_gate as gate};
    // Non-session methods never refresh, regardless of BYOK status or endpoint.
    for fp in [false, true] {
        assert!(!gate(false, ModelByok::NotByok, fp));
        assert!(!gate(false, ModelByok::Byok, fp));
        assert!(!gate(false, ModelByok::Unknown, fp));
        // Session method: a definite classification ignores the endpoint —
        // NotByok always refreshes (only ever routes to the session endpoint),
        // a genuine per-model Byok never does.
        assert!(gate(true, ModelByok::NotByok, fp));
        assert!(!gate(true, ModelByok::Byok, fp));
    }
    // Session method + Unknown BYOK: refresh only against a first-party xAI
    // host, so a transiently-unclassifiable config can't demote a live session
    // (the stale-token 401 regression) yet the session token never leaks to a
    // third-party BYOK endpoint. This arm was unconditionally `false` pre-fix.
    assert!(gate(true, ModelByok::Unknown, true));
    assert!(!gate(true, ModelByok::Unknown, false));
}

/// Pre-fix, the gate read `auth_type` and skipped recovery here, 401'ing every
/// turn until restart.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn sampler_401_session_method_with_stale_api_key_auth_type_still_recovers() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::ApiKey,
                "stale-session-jwt".to_string(),
            )
            .await;

            let result = actor.handle_sampling_failure(auth_error()).await;

            assert!(
                matches!(
                    result,
                    Ok(SamplerFailureRecovery::RefreshAuthAndResubmit { .. })
                ),
                "session-based method must recover even when auth_type transiently reads ApiKey"
            );
            assert!(
                called.load(Ordering::SeqCst),
                "the OIDC refresher must be invoked for a session-based method"
            );
        })
        .await;
}

/// Same regression via the `oidc` method id (the other session-based variant).
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn sampler_401_oidc_method_with_stale_api_key_auth_type_still_recovers() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "oidc",
                xai_chat_state::AuthType::ApiKey,
                "stale-session-jwt".to_string(),
            )
            .await;

            let result = actor.handle_sampling_failure(auth_error()).await;

            assert!(
                matches!(
                    result,
                    Ok(SamplerFailureRecovery::RefreshAuthAndResubmit { .. })
                ),
                "oidc method must recover even when auth_type transiently reads ApiKey"
            );
            assert!(
                called.load(Ordering::SeqCst),
                "the OIDC refresher must be invoked"
            );
        })
        .await;
}

/// Regression: when the active model routes to the ChatGPT Codex backend,
/// `reconstruct_full_config` must wire the OpenAI Codex bearer resolver even
/// if the ACP session method is still `cached_token` / OIDC. Preferring the
/// xAI AuthManager resolver previously sent the session JWT to
/// `chatgpt.com/backend-api/codex/responses` → 401 "Could not parse your
/// authentication token" with a false "auth recovery succeeded" loop.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn reconstruct_full_config_prefers_codex_resolver_over_session_auth_manager() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let auth_path = dir.path().join("auth.json");
            let _guard =
                xai_grok_test_support::EnvGuard::set("GROK_AUTH_PATH", auth_path.to_str().unwrap());
            // Minimal valid Codex JWT payload with chatgpt_account_id claim.
            use base64::Engine as _;
            let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-test"}}"#);
            let access = format!("hdr.{payload}.sig");
            let auth = crate::auth::GrokAuth {
                key: access.clone(),
                auth_mode: crate::auth::AuthMode::OpenAiCodex,
                account_id: Some("acct-test".into()),
                refresh_token: Some("rt".into()),
                expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                ..crate::auth::GrokAuth::test_default()
            };
            crate::auth::store_openai_codex_auth(dir.path(), &auth).unwrap();

            let (_am_dir, am) = auth_manager_with_valid_token("xai-session-jwt");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "xai-session-jwt".to_string(),
            )
            .await;

            // Point the session sampling config at the Codex backend.
            if let Some(mut cfg) = actor.chat_state_handle.get_sampling_config().await {
                cfg.base_url = "https://chatgpt.com/backend-api/codex".into();
                cfg.model = "gpt-5.6-sol".into();
                actor.chat_state_handle.update_sampling_config(cfg);
            }

            let cfg = actor.reconstruct_full_config().await;
            assert!(
                cfg.bearer_resolver.is_some(),
                "openai-codex base_url must install a live bearer resolver"
            );
            assert!(
                cfg.api_key.is_none(),
                "resolver-backed Codex auth must not stamp an eager access token"
            );
            // The live resolver must surface the Codex bearer (not AuthManager).
            // Keep this as a boolean assertion so a parallel-test regression
            // can never print a developer credential into the test log.
            let live = cfg
                .bearer_resolver
                .as_ref()
                .and_then(|r| r.current_bearer());
            assert!(
                live.as_deref() == Some(access.as_str()),
                "live resolver returned the wrong credential source"
            );
            assert!(
                cfg.responses_codex_dialect,
                "Codex dialect flag must be set for the chatgpt backend"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn reconstruct_full_config_uses_catalog_platform_on_shared_oauth_proxy() {
    let proxy = "https://oauth-proxy.example.test/v1";
    let _kimi_base =
        xai_grok_test_support::EnvGuard::set(xai_grok_models::KIMI_CODE_BASE_URL_ENV, proxy);
    let _codex_base = xai_grok_test_support::EnvGuard::set("GROK_OPENAI_CODEX_BASE_URL", proxy);
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            for (platform, model, resolver_name) in [
                (
                    xai_grok_models::PlatformId::KimiCode,
                    "kimi-for-coding",
                    "KimiCodeBearerResolver",
                ),
                (
                    xai_grok_models::PlatformId::OpenAiCodex,
                    "gpt-5.1-codex",
                    "OpenAiCodexBearerResolver",
                ),
            ] {
                let (actor, _rx) = make_actor_with_method_and_credentials(
                    None,
                    "cached_token",
                    xai_chat_state::AuthType::SessionToken,
                    "stale-catalog-token".to_string(),
                )
                .await;
                if let Some(mut sampling) = actor.chat_state_handle.get_sampling_config().await {
                    sampling.base_url = proxy.to_string();
                    sampling.model = model.to_string();
                    actor.chat_state_handle.update_sampling_config(sampling);
                }
                actor
                    .model_auth_memo
                    .replace(Some(crate::session::acp_session::ModelAuthMemo {
                        model_id: model.to_string(),
                        facts: crate::agent::config::ModelAuthFacts {
                            byok: crate::agent::auth_method::ModelByok::NotByok,
                            auth_scheme: Default::default(),
                            oauth_platform: Some(platform),
                            platform_oauth_active: true,
                        },
                        provider: None,
                        catalog_epoch: 0,
                    }));

                let config = actor.reconstruct_full_config().await;
                let resolver = config
                    .bearer_resolver
                    .as_ref()
                    .expect("catalog OAuth platform must install its live resolver");
                assert!(format!("{resolver:?}").contains(resolver_name));
                assert!(config.api_key.is_none());
                assert_eq!(
                    config.responses_codex_dialect,
                    platform == xai_grok_models::PlatformId::OpenAiCodex
                );
                assert_eq!(
                    config.kimi_dialect,
                    platform == xai_grok_models::PlatformId::KimiCode
                );
                if platform == xai_grok_models::PlatformId::KimiCode {
                    assert!(!config.extra_headers.contains_key("OpenAI-Beta"));
                    assert!(!config.extra_headers.contains_key("originator"));
                } else {
                    assert!(config.extra_headers.contains_key("OpenAI-Beta"));
                    assert!(config.extra_headers.contains_key("originator"));
                    assert!(!config.extra_headers.contains_key("anthropic-version"));
                    assert!(!config.extra_headers.contains_key("X-Msh-Device-Id"));
                }
            }
        })
        .await;
}

/// Build a live-catalog Kimi entry for session reconstruct / 401 tests.
/// Facts must come from `live_catalog_auth_facts` (not a hand-written memo).
fn kimi_test_catalog_entry(
    catalog_id: &str,
    model_slug: &str,
    api_key: Option<&str>,
    platform_oauth_active: bool,
) -> crate::agent::config::ModelEntry {
    let mut entry = crate::agent::config::ModelEntry::fallback(
        model_slug,
        &crate::agent::config::EndpointsConfig::default(),
    );
    entry.info.id = Some(catalog_id.to_string());
    entry.info.model = model_slug.to_string();
    entry.info.base_url = "https://api.kimi.com/coding/v1".to_string();
    entry.api_key = api_key.map(str::to_string);
    entry.platform_oauth_active = platform_oauth_active;
    entry
}

/// Regression: hybrid Kimi static API key must not install
/// `KimiCodeBearerResolver` or drop the stamped key for an xAI session bearer.
///
/// a7ae611 made Kimi identity-only (like Codex) and stripped static keys on
/// every `kimi-code/*` reconstruct. Static path is `Byok && !platform_oauth_active`:
/// keep the key, strip x-msh-device-*, retain anthropic-version.
///
/// Uses `insert_test_entry` so `model_auth_facts` walks live catalog facts
/// (not a hand-written `ModelAuthMemo`).
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn reconstruct_full_config_static_kimi_keeps_api_key_without_resolver() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let auth_path = dir.path().join("auth.json");
            let _auth_guard =
                xai_grok_test_support::EnvGuard::set("GROK_AUTH_PATH", auth_path.to_str().unwrap());
            // Isolate from a developer shell that may set KIMI_CODE_API_KEY.
            let _clear_kimi =
                xai_grok_test_support::EnvGuard::unset(xai_grok_models::KIMI_CODE_API_KEY_ENV);

            let catalog_id = "kimi-code/kimi-static-test";
            let static_key = "static-kimi-live-catalog-key";
            let (actor, _rx) = make_actor_with_method_and_credentials(
                None,
                "xai.api_key",
                xai_chat_state::AuthType::ApiKey,
                // Chat-state may hold a different seed; live catalog own key wins.
                "stale-chat-state-seed".to_string(),
            )
            .await;
            if let Some(mut sampling) = actor.chat_state_handle.get_sampling_config().await {
                sampling.base_url = "https://api.kimi.com/coding/v1".to_string();
                sampling.model = catalog_id.to_string();
                // Seed device headers the way a prior OAuth turn / catalog stamp
                // might leave them; reconstruct must strip them for static mode.
                sampling
                    .extra_headers
                    .insert("x-msh-device-id".to_string(), "stale-device-id".to_string());
                sampling.extra_headers.insert(
                    "x-msh-device-name".to_string(),
                    "stale-device-name".to_string(),
                );
                sampling.extra_headers.insert(
                    "x-msh-device-model".to_string(),
                    "stale-device-model".to_string(),
                );
                actor.chat_state_handle.update_sampling_config(sampling);
            }

            let entry =
                kimi_test_catalog_entry(catalog_id, "kimi-for-coding", Some(static_key), false);
            assert!(
                entry.has_own_credentials() && !entry.platform_oauth_active,
                "fixture must be static own-key hybrid path"
            );
            actor.models_manager.insert_test_entry(catalog_id, entry);
            // Ensure we do not short-circuit via a leftover memo from setup.
            actor.invalidate_model_auth_memo();

            let config = actor.reconstruct_full_config().await;
            assert!(
                config.bearer_resolver.is_none(),
                "static Kimi must not install KimiCodeBearerResolver"
            );
            assert_eq!(
                config.api_key.as_deref(),
                Some(static_key),
                "static Kimi must prefer live catalog own key (not strip for OAuth resolver)"
            );
            assert!(
                config.extra_headers.keys().all(|name| {
                    !name.eq_ignore_ascii_case("x-msh-device-id")
                        && !name.eq_ignore_ascii_case("x-msh-device-name")
                        && !name.eq_ignore_ascii_case("x-msh-device-model")
                }),
                "static Kimi must not send OAuth device identity headers: {:?}",
                config.extra_headers.keys().collect::<Vec<_>>()
            );
            assert_eq!(
                config
                    .extra_headers
                    .get("anthropic-version")
                    .map(String::as_str),
                Some(xai_grok_models::ANTHROPIC_VERSION_HEADER_VALUE),
                "static Kimi Messages route still needs anthropic-version"
            );
            assert!(
                config.kimi_dialect,
                "Kimi dialect shaping still applies on the static path"
            );
        })
        .await;
}

/// Pre-login / NotByok Kimi still installs the live resolver from catalog
/// identity so post-`/login kimi` turns do not stick on a stale
/// `platform_oauth_active = false` memo.
///
/// Live catalog entry (no own credential, oauth stamp inactive) — not a
/// hand-written memo — so `live_catalog_auth_facts` produces NotByok.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn reconstruct_full_config_installs_kimi_resolver_when_not_byok_pre_login() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let auth_path = dir.path().join("auth.json");
            let _auth_guard =
                xai_grok_test_support::EnvGuard::set("GROK_AUTH_PATH", auth_path.to_str().unwrap());
            let _clear_kimi =
                xai_grok_test_support::EnvGuard::unset(xai_grok_models::KIMI_CODE_API_KEY_ENV);

            let catalog_id = "kimi-code/kimi-prelogin-test";
            let (actor, _rx) = make_actor_with_method_and_credentials(
                None,
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "stale-session-token".to_string(),
            )
            .await;
            if let Some(mut sampling) = actor.chat_state_handle.get_sampling_config().await {
                sampling.base_url = "https://api.kimi.com/coding/v1".to_string();
                sampling.model = catalog_id.to_string();
                actor.chat_state_handle.update_sampling_config(sampling);
            }

            let entry = kimi_test_catalog_entry(catalog_id, "kimi-for-coding", None, false);
            assert!(
                !entry.has_own_credentials() && !entry.platform_oauth_active,
                "fixture must be pre-login NotByok hybrid path"
            );
            actor.models_manager.insert_test_entry(catalog_id, entry);
            actor.invalidate_model_auth_memo();

            let config = actor.reconstruct_full_config().await;
            let resolver = config
                .bearer_resolver
                .as_ref()
                .expect("pre-login Kimi NotByok must install KimiCodeBearerResolver");
            assert!(
                format!("{resolver:?}").contains("KimiCodeBearerResolver"),
                "expected KimiCodeBearerResolver, got {resolver:?}"
            );
            assert!(
                config.api_key.is_none(),
                "OAuth Kimi path must not fall through to the session/api_key stamp"
            );
        })
        .await;
}

/// Static Kimi 401 must not enter `force_refresh_kimi_code_auth` / xAI recovery.
///
/// Live catalog Byok + !platform_oauth_active drives the predicate (same path
/// production uses after model_auth_memo is cold).
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn sampler_401_static_kimi_skips_oauth_refresh() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let auth_path = dir.path().join("auth.json");
            let _auth_guard =
                xai_grok_test_support::EnvGuard::set("GROK_AUTH_PATH", auth_path.to_str().unwrap());
            let _clear_kimi = xai_grok_test_support::EnvGuard::unset(
                xai_grok_models::KIMI_CODE_API_KEY_ENV,
            );

            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let catalog_id = "kimi-code/kimi-static-401";
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::ApiKey,
                "static-kimi-bad-key".to_string(),
            )
            .await;
            if let Some(mut sampling) = actor.chat_state_handle.get_sampling_config().await {
                sampling.base_url = "https://api.kimi.com/coding/v1".to_string();
                sampling.model = catalog_id.to_string();
                actor.chat_state_handle.update_sampling_config(sampling);
            }

            let entry = kimi_test_catalog_entry(
                catalog_id,
                "kimi-for-coding",
                Some("static-kimi-bad-key"),
                false,
            );
            assert!(entry.has_own_credentials() && !entry.platform_oauth_active);
            actor.models_manager.insert_test_entry(catalog_id, entry);
            actor.invalidate_model_auth_memo();

            let result = actor.handle_sampling_failure(auth_error()).await;
            let err = match result {
                Err(e) => e,
                Ok(_) => {
                    panic!("static Kimi 401 must surface a terminal error, not OAuth/xAI retry")
                }
            };
            assert!(
                !called.load(Ordering::SeqCst),
                "static Kimi 401 must NOT trigger xAI session-token refresh"
            );
            let data = err.data.expect("error data");
            let msg = data
                .as_str()
                .or_else(|| data.get("message").and_then(|v| v.as_str()))
                .unwrap_or_default();
            assert!(
                msg.contains("static API key")
                    || msg.contains(xai_grok_models::KIMI_CODE_API_KEY_ENV),
                "terminal message should point at the static Kimi key setup, got: {msg}"
            );
            assert!(
                msg.contains("/login kimi") || msg.contains("grok login --kimi"),
                "terminal message should mention real TUI `/login kimi` or CLI `grok login --kimi`, got: {msg}"
            );
            assert!(
                !msg.contains("/login --kimi"),
                "must not advertise non-existent TUI flag `/login --kimi`, got: {msg}"
            );
        })
        .await;
}

/// Regression: install `OpenAiCodexBearerResolver` from catalog identity alone.
///
/// A session that selected `openai-codex/*` *before* `/login` memoizes
/// `platform_oauth_active = false`. Gating the resolver on that flag (post-r7)
/// sent requests with no Authorization → 401 uncharged resubmit loop.
#[tokio::test(flavor = "current_thread")]
async fn reconstruct_full_config_installs_codex_resolver_when_platform_oauth_inactive() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let model = "gpt-5.1-codex";
            let (actor, _rx) = make_actor_with_method_and_credentials(
                None,
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "stale-session-token".to_string(),
            )
            .await;
            if let Some(mut sampling) = actor.chat_state_handle.get_sampling_config().await {
                sampling.base_url = "https://chatgpt.com/backend-api/codex".to_string();
                sampling.model = model.to_string();
                sampling.adapter_kind = xai_grok_models::AdapterKind::OpenAiCodex;
                actor.chat_state_handle.update_sampling_config(sampling);
            }
            // Pre-login memo: catalog id is OpenAiCodex but stamp not yet active.
            actor
                .model_auth_memo
                .replace(Some(crate::session::acp_session::ModelAuthMemo {
                    model_id: model.to_string(),
                    facts: crate::agent::config::ModelAuthFacts {
                        byok: crate::agent::auth_method::ModelByok::NotByok,
                        auth_scheme: Default::default(),
                        oauth_platform: Some(xai_grok_models::PlatformId::OpenAiCodex),
                        platform_oauth_active: false,
                    },
                    provider: None,
                    catalog_epoch: 0,
                }));

            let config = actor.reconstruct_full_config().await;
            let resolver = config
                .bearer_resolver
                .as_ref()
                .expect("codex catalog identity must install OpenAiCodexBearerResolver even when platform_oauth_active is false");
            assert!(
                format!("{resolver:?}").contains("OpenAiCodexBearerResolver"),
                "expected OpenAiCodexBearerResolver, got {resolver:?}"
            );
            assert!(
                config.api_key.is_none(),
                "codex oauth route must not fall through to the session/api_key stamp"
            );
            assert!(config.responses_codex_dialect);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn reconstruct_full_config_distinguishes_copilot_oauth_marker_from_static_token() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            for (platform_oauth_active, expected_resolver) in [(true, true), (false, false)] {
                let model = if platform_oauth_active {
                    "copilot-oauth-model"
                } else {
                    "copilot-static-model"
                };
                let credential = if platform_oauth_active {
                    "stale-catalog-marker"
                } else {
                    "static-copilot-token"
                };
                let (actor, _rx) = make_actor_with_method_and_credentials(
                    None,
                    "cached_token",
                    xai_chat_state::AuthType::ApiKey,
                    credential.to_string(),
                )
                .await;
                if let Some(mut sampling) = actor.chat_state_handle.get_sampling_config().await {
                    sampling.model = model.to_string();
                    sampling.adapter_kind = xai_grok_models::AdapterKind::GitHubCopilot;
                    actor.chat_state_handle.update_sampling_config(sampling);
                }
                actor
                    .model_auth_memo
                    .replace(Some(crate::session::acp_session::ModelAuthMemo {
                        model_id: model.to_string(),
                        facts: crate::agent::config::ModelAuthFacts {
                            byok: crate::agent::auth_method::ModelByok::Byok,
                            auth_scheme: Default::default(),
                            oauth_platform: None,
                            platform_oauth_active,
                        },
                        provider: None,
                        catalog_epoch: 0,
                    }));

                let config = actor.reconstruct_full_config().await;
                assert_eq!(config.bearer_resolver.is_some(), expected_resolver);
                if expected_resolver {
                    assert!(config.api_key.is_none());
                    assert!(
                        format!("{:?}", config.bearer_resolver.as_ref().unwrap())
                            .contains("GitHubCopilotBearerResolver")
                    );
                } else {
                    assert_eq!(config.api_key.as_deref(), Some("static-copilot-token"));
                }
            }
        })
        .await;
}

/// Regression: chat-state still holds the xAI session JWT after switching to
/// a managed API-key model (`ollama/*`) whose base_url is *local* (not in the
/// open-platform host list). Catalog-id detection must keep the session
/// resolver off and must not send the session JWT as the platform key.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn reconstruct_full_config_managed_ollama_catalog_drops_session_jwt() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let auth_path = dir.path().join("auth.json");
            let _auth_guard =
                xai_grok_test_support::EnvGuard::set("GROK_AUTH_PATH", auth_path.to_str().unwrap());
            let _ollama_key =
                xai_grok_test_support::EnvGuard::set("OLLAMA_API_KEY", "ollama-live-env-key");

            let (_am_dir, am) = auth_manager_with_valid_token("xai-session-jwt");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                // Stale: previous first-party turn left the session JWT here.
                "xai-session-jwt".to_string(),
            )
            .await;

            if let Some(mut cfg) = actor.chat_state_handle.get_sampling_config().await {
                // Local Ollama is NOT matched by open_platform_endpoint host list.
                cfg.base_url = "http://127.0.0.1:11434/v1".into();
                cfg.model = "ollama/deepseek-v4-flash".into();
                actor.chat_state_handle.update_sampling_config(cfg);
            }

            // Seed the live catalog with a managed ollama entry that owns env_key.
            let mut entry = crate::agent::config::ModelEntry::fallback(
                "deepseek-v4-flash",
                &crate::agent::config::EndpointsConfig::default(),
            );
            entry.info.id = Some("ollama/deepseek-v4-flash".into());
            entry.info.base_url = "http://127.0.0.1:11434/v1".into();
            entry.env_key = Some(crate::agent::config::EnvKeys::new(["OLLAMA_API_KEY"]));
            assert!(
                entry.has_own_credentials(),
                "test fixture must resolve OLLAMA_API_KEY"
            );
            actor
                .models_manager
                .insert_test_entry("ollama/deepseek-v4-flash", entry);

            let cfg = actor.reconstruct_full_config().await;
            assert!(
                cfg.bearer_resolver.is_none(),
                "managed ollama/* must not install the xAI session bearer resolver"
            );
            assert_eq!(
                cfg.api_key.as_deref(),
                Some("ollama-live-env-key"),
                "must prefer live OLLAMA_API_KEY over the stale session JWT in chat-state"
            );
        })
        .await;
}

/// Regression: a live-only open-platform model (e.g. `ollama/glm-5.2` from
/// the platform `/models` sync) is absent from the offline catalog the BYOK
/// gate consults, so it classifies `NotByok` and the session gate activates.
/// `reconstruct_full_config` must still keep the xAI session bearer resolver
/// OFF the third-party endpoint — the stamped platform key in chat-state
/// credentials is the correct signer. Previously the AuthManager resolver
/// replaced it and the request went out with the xAI session JWT →
/// `ollama.com` 401 → false "auth recovery succeeded" loop.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn reconstruct_full_config_no_session_resolver_for_open_platform_endpoint() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let auth_path = dir.path().join("auth.json");
            // Isolate home + auth so endpoint re-resolve cannot pick up the
            // developer machine's real Ollama/OpenCode keys from ~/.grok.
            let _home_guard =
                xai_grok_test_support::EnvGuard::set("GROK_HOME", dir.path().to_str().unwrap());
            let _auth_guard =
                xai_grok_test_support::EnvGuard::set("GROK_AUTH_PATH", auth_path.to_str().unwrap());
            let _byok = xai_grok_test_support::unset_all_byok_platform_api_key_envs();

            let (_am_dir, am) = auth_manager_with_valid_token("xai-session-jwt");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::ApiKey,
                "ollama-platform-key".to_string(),
            )
            .await;

            // Point the session sampling config at the open-platform endpoint.
            // The slug is deliberately absent from every offline catalog (a
            // stand-in for live-only entries like `ollama/glm-5.2`), so the
            // BYOK gate classifies `NotByok` regardless of the dev box's env
            // — no env mutation needed, which would race parallel tests.
            if let Some(mut cfg) = actor.chat_state_handle.get_sampling_config().await {
                cfg.base_url = "https://ollama.com/v1".into();
                cfg.model = "ollama-live-only-model-not-in-offline-catalog".into();
                actor.chat_state_handle.update_sampling_config(cfg);
            }

            let cfg = actor.reconstruct_full_config().await;
            assert!(
                cfg.bearer_resolver.is_none(),
                "open-platform endpoint must not wire the xAI session bearer resolver"
            );
            assert_eq!(
                cfg.api_key.as_deref(),
                Some("ollama-platform-key"),
                "must sign with the stamped platform key, not the xAI session JWT"
            );
        })
        .await;
}

/// Regression: `OLLAMA_API_KEY` is set and bare wire model is `deepseek-v4-flash`.
/// Catalog bare-slug lookup used to prefer Ollama's env key for OpenCode Go
/// requests → 401 Invalid API key. Endpoint re-resolve must win.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn reconstruct_full_config_opencode_go_ignores_ollama_env_for_bare_slug() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            crate::auth::store_platform_api_key(
                dir.path(),
                "opencode",
                "sk-opencode-go-correct-key",
                None,
            )
            .unwrap();
            let _home = xai_grok_test_support::EnvGuard::set(
                "GROK_HOME",
                dir.path().to_str().unwrap(),
            );
            let _byok = xai_grok_test_support::unset_all_byok_platform_api_key_envs();
            // Pollute with the real failure mode from production logs.
            let _ollama = xai_grok_test_support::EnvGuard::set(
                "OLLAMA_API_KEY",
                "e122cf83c6bf4e2e9c173c3bebd9234d.-oiBCiRY52XQ7ieT6_ybgKK3",
            );

            let (_am_dir, am) = auth_manager_with_valid_token("eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJ4YWkifQ.sig");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "oidc",
                xai_chat_state::AuthType::SessionToken,
                "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJzdGFsZSJ9.stale".to_string(),
            )
            .await;

            if let Some(mut cfg) = actor.chat_state_handle.get_sampling_config().await {
                cfg.base_url = xai_grok_models::OPENCODE_GO_BASE_URL_DEFAULT.into();
                cfg.model = "deepseek-v4-flash".into();
                actor.chat_state_handle.update_sampling_config(cfg);
            }

            let cfg = actor.reconstruct_full_config().await;
            assert_eq!(
                cfg.api_key.as_deref(),
                Some("sk-opencode-go-correct-key"),
                "must not send OLLAMA_API_KEY to OpenCode Go"
            );
            assert!(
                !cfg.api_key
                    .as_deref()
                    .is_some_and(|k| k.contains("7ieT6_ybgKK3")),
                "leaked ollama key"
            );
        })
        .await;
}

/// Regression (OpenCode Go): wire model is bare `deepseek-v4-flash` while chat
/// state still holds a prior xAI OIDC JWT. Reconstruct must re-resolve the
/// platform key from `https://opencode.ai/zen/go/v1` (auth.json
/// `platform/opencode`), not forward the JWT (→ 401 Invalid API key).
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn reconstruct_full_config_opencode_go_uses_platform_key_not_stale_jwt() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            // Store under the OpenCode credential group (canonical for opencode-go).
            crate::auth::store_platform_api_key(
                dir.path(),
                "opencode",
                "sk-opencode-go-test-key",
                None,
            )
            .unwrap();
            let _home = xai_grok_test_support::EnvGuard::set(
                "GROK_HOME",
                dir.path().to_str().unwrap(),
            );
            let _byok = xai_grok_test_support::unset_all_byok_platform_api_key_envs();

            // Stale JWT left over from a prior first-party turn in chat-state.
            let stale_jwt = "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJzdGFsZSJ9.stale_sig_suffix_xx";
            let (_am_dir, am) = auth_manager_with_valid_token("eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJjdXJyZW50In0.current_sig");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "oidc",
                xai_chat_state::AuthType::SessionToken,
                stale_jwt.to_string(),
            )
            .await;

            if let Some(mut cfg) = actor.chat_state_handle.get_sampling_config().await {
                cfg.base_url = xai_grok_models::OPENCODE_GO_BASE_URL_DEFAULT.into();
                // Bare wire id — same shape as sampling_config_for_model().
                cfg.model = "deepseek-v4-flash".into();
                actor.chat_state_handle.update_sampling_config(cfg);
            }

            let cfg = actor.reconstruct_full_config().await;
            assert!(
                cfg.bearer_resolver.is_none(),
                "OpenCode Go must not install xAI session resolver"
            );
            assert_eq!(
                cfg.api_key.as_deref(),
                Some("sk-opencode-go-test-key"),
                "must re-resolve platform/opencode key from endpoint, not stale JWT"
            );
        })
        .await;
}

/// Regression: a 401 from an open-platform endpoint must NOT run the xAI
/// session-token recovery. The refresher "succeeds" on the wrong credential
/// and the retry re-sends it, looping to the misleading "Auth recovery
/// succeeded but inference request was still rejected (401)" terminal error.
/// The 401 surfaces directly with the platform's setup hint instead.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn sampler_401_on_open_platform_endpoint_skips_session_recovery() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::ApiKey,
                "ollama-platform-key".to_string(),
            )
            .await;
            if let Some(mut cfg) = actor.chat_state_handle.get_sampling_config().await {
                cfg.base_url = "https://ollama.com/v1".into();
                cfg.model = "ollama-live-only-model-not-in-offline-catalog".into();
                actor.chat_state_handle.update_sampling_config(cfg);
            }

            let result = actor.handle_sampling_failure(auth_error()).await;

            let Err(err) = result else {
                panic!(
                    "open-platform 401 must surface as a terminal error, not xAI session recovery"
                );
            };
            assert!(
                !called.load(Ordering::SeqCst),
                "xAI session refresher must not run for a third-party BYOK endpoint"
            );
            let rendered = format!("{err:?}");
            assert!(
                rendered.contains("Ollama Cloud"),
                "terminal error should name the platform setup hint, got: {rendered}"
            );
        })
        .await;
}

/// Without the live bearer resolver here the sampler would sign requests with
/// the stale buffered token.
#[tokio::test(flavor = "current_thread")]
async fn reconstruct_full_config_wires_bearer_resolver_for_session_method_despite_api_key_auth_type()
 {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("fresh-session-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::ApiKey,
                "stale-session-jwt".to_string(),
            )
            .await;

            let cfg = actor.reconstruct_full_config().await;

            assert!(
                cfg.bearer_resolver.is_some(),
                "session-based method must use the live bearer resolver, not the buffered key"
            );
        })
        .await;
}

/// Negative: a genuine `xai.api_key` method keeps its configured key on the
/// wire (no live resolver).
#[tokio::test(flavor = "current_thread")]
async fn reconstruct_full_config_no_bearer_resolver_for_api_key_method() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("session-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "xai.api_key",
                xai_chat_state::AuthType::ApiKey,
                "xai-static-key".to_string(),
            )
            .await;

            let cfg = actor.reconstruct_full_config().await;

            assert!(
                cfg.bearer_resolver.is_none(),
                "api-key method must keep its configured bearer (no live resolver)"
            );
        })
        .await;
}

/// The pre-flight refresh heals a transiently-`ApiKey` session by writing the
/// fresh session token back into `creds.api_key`.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn pre_flight_refresh_heals_session_method_with_stale_api_key_auth_type() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("fresh-session-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::ApiKey,
                "stale-session-jwt".to_string(),
            )
            .await;

            actor.refresh_token_if_expired().await;

            assert_eq!(
                actor
                    .chat_state_handle
                    .get_credentials()
                    .await
                    .api_key
                    .as_deref(),
                Some("fresh-session-token"),
                "session-based pre-flight refresh must heal a stale api_key with the live token"
            );
        })
        .await;
}

/// End-to-end for the frozen-gate bug: a session born on `xai.api_key` (gate
/// inactive) must adopt a later OIDC `/login` on the SAME actor -- the shared
/// `auth_method_id` handle is flipped in place (no re-spawn), so the next turn
/// wires the live bearer resolver and heals the stale key.
#[tokio::test(flavor = "current_thread")]
async fn session_born_on_api_key_recovers_after_oidc_login_without_restart() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("fresh-oidc-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "xai.api_key",
                xai_chat_state::AuthType::ApiKey,
                "stale-session-jwt".to_string(),
            )
            .await;

            // Born on api_key: the gate is inactive, so no live resolver.
            assert!(
                actor
                    .reconstruct_full_config()
                    .await
                    .bearer_resolver
                    .is_none(),
                "api-key session must not use the live resolver before login"
            );

            // Simulate the agent's `authenticate` publishing an OIDC method into
            // the shared handle this running actor already holds (no re-spawn).
            actor
                .auth_method_id
                .store(Some(std::sync::Arc::new(acp::AuthMethodId::new("oidc"))));

            // The gate is recomputed each turn from the shared handle, so the
            // flip alone activates the live resolver on the very next turn --
            // no re-spawn, before any token refresh runs.
            assert!(
                actor
                    .reconstruct_full_config()
                    .await
                    .bearer_resolver
                    .is_some(),
                "flipping the shared handle activates the resolver on the next turn"
            );

            // The pre-flight refresh then heals the stale api_key with the live token.
            actor.refresh_token_if_expired().await;
            assert_eq!(
                actor
                    .chat_state_handle
                    .get_credentials()
                    .await
                    .api_key
                    .as_deref(),
                Some("fresh-oidc-token"),
                "the stale api_key must be healed with the fresh OIDC token"
            );
        })
        .await;
}

// Per-model BYOK memo (`SessionActor::model_auth_memo`): a definite cached
// status is served without recomputing, and the memo keys on `model_id`.

/// The cache-hit branch is what lets a later config parse failure (`Unknown`)
/// fall back to the last-known-good status.
#[tokio::test(flavor = "current_thread")]
async fn model_auth_memo_serves_cached_status_and_keys_on_model() {
    use crate::agent::auth_method::ModelByok;
    use crate::agent::config::ModelAuthFacts;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = make_actor_with_method_and_credentials(
                None,
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "k".to_string(),
            )
            .await;

            actor
                .model_auth_memo
                .replace(Some(crate::session::acp_session::ModelAuthMemo {
                    model_id: "model-a".to_string(),
                    facts: ModelAuthFacts {
                        byok: ModelByok::Byok,
                        auth_scheme: Default::default(),
                        oauth_platform: None,
                        platform_oauth_active: false,
                    },
                    provider: None,
                    catalog_epoch: 0,
                }));

            // Cache hit: served without consulting config.
            assert_eq!(actor.model_auth_facts("model-a").byok, ModelByok::Byok);

            // Different model re-resolves rather than serving the stale `Byok`.
            assert_ne!(actor.model_auth_facts("model-b").byok, ModelByok::Byok);
        })
        .await;
}

/// A session method whose active model is a genuine per-model BYOK model keeps
/// the model's own key on the wire (no live resolver).
#[tokio::test(flavor = "current_thread")]
async fn reconstruct_full_config_no_bearer_resolver_for_byok_model_on_session_method() {
    use crate::agent::auth_method::ModelByok;
    use crate::agent::config::ModelAuthFacts;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("session-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "byok-key".to_string(),
            )
            .await;

            let model = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| c.model)
                .unwrap_or_default();
            actor
                .model_auth_memo
                .replace(Some(crate::session::acp_session::ModelAuthMemo {
                    model_id: model,
                    facts: ModelAuthFacts {
                        byok: ModelByok::Byok,
                        auth_scheme: Default::default(),
                        oauth_platform: None,
                        platform_oauth_active: false,
                    },
                    provider: None,
                    catalog_epoch: 0,
                }));

            let cfg = actor.reconstruct_full_config().await;

            assert!(
                cfg.bearer_resolver.is_none(),
                "a per-model BYOK model must keep its own key even on a session method"
            );
        })
        .await;
}

/// Regression: a model-switch chokepoint must invalidate
/// the memo even when `model_id` is unchanged. Otherwise a config edit that
/// turns the current model into a per-model BYOK model on a third-party
/// `base_url` keeps serving the stale `NotByok`, leaving the gate active and
/// leaking the OIDC token cross-host.
#[tokio::test(flavor = "current_thread")]
async fn set_session_model_invalidates_byok_memo_for_same_model_id() {
    use crate::agent::auth_method::ModelByok;
    use crate::agent::config::ModelAuthFacts;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = make_actor_with_method_and_credentials(
                None,
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "k".to_string(),
            )
            .await;

            let model = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| c.model)
                .unwrap_or_default();

            actor
                .model_auth_memo
                .replace(Some(crate::session::acp_session::ModelAuthMemo {
                    model_id: model.clone(),
                    facts: ModelAuthFacts {
                        byok: ModelByok::NotByok,
                        auth_scheme: Default::default(),
                        oauth_platform: None,
                        platform_oauth_active: false,
                    },
                    provider: None,
                    catalog_epoch: 0,
                }));

            // Switch to the same model_id, now a per-model BYOK model on a
            // third-party endpoint.
            let cfg = xai_grok_sampler::SamplerConfig {
                api_key: Some("byok-key".to_string()),
                base_url: "https://third-party.example/v1".to_string(),
                model: model.clone(),
                max_completion_tokens: None,
                temperature: None,
                top_p: None,
                api_backend: crate::sampling::ApiBackend::ChatCompletions,
                adapter_kind: Default::default(),
                request_compat: None,
                endpoint_path: None,
                auth_scheme: Default::default(),
                extra_headers: Default::default(),
                query_params: Default::default(),
                env_http_headers: Default::default(),
                context_window: 256_000,
                client_version: None,
                force_http1: false,
                max_retries: None,
                stream_tool_calls: false,
                idle_timeout_secs: None,
                client_identifier: None,
                reasoning_effort: None,
                deployment_id: None,
                user_id: None,
                origin_client: None,
                attribution_callback: None,
                bearer_resolver: None,
                supports_backend_search: false,
                compactions_remaining: None,
                compaction_at_tokens: None,
                doom_loop_recovery: None,
                header_injector: None,
                responses_codex_dialect: false,
                bedrock_request_metadata: Default::default(),
                bedrock_headers: Default::default(),
                bedrock_profile: None,
                kimi_dialect: false,
            };
            let _ = actor
                .handle_set_session_model(cfg, false, false, true, 85)
                .await;

            assert!(
                actor.model_auth_memo.borrow().is_none(),
                "a model switch must invalidate the per-model BYOK memo so the next \
                 reconstruct recomputes under the current config"
            );
        })
        .await;
}

use crate::auth::test_counting_provider as counting_provider;

/// Seed the per-model memo so `model_auth_provider` resolves without a
/// config load.
async fn seed_provider_memo(actor: &Arc<SessionActor>, provider: crate::auth::AuthProviderRef) {
    let model = actor
        .chat_state_handle
        .get_sampling_config()
        .await
        .map(|c| c.model)
        .unwrap_or_default();
    actor
        .model_auth_memo
        .replace(Some(crate::session::acp_session::ModelAuthMemo {
            model_id: model,
            facts: crate::agent::config::ModelAuthFacts {
                byok: crate::agent::auth_method::ModelByok::Byok,
                auth_scheme: Default::default(),
                oauth_platform: None,
                platform_oauth_active: false,
            },
            provider: Some(provider),
            catalog_epoch: 0,
        }));
}

/// Regression: switching from a provider-backed model to a first-party model
/// must drop the minted provider token from the chat credentials, so it can
/// never ride a later request to `api.x.ai`. Mirrors the forward direction in
/// `set_session_model_invalidates_byok_memo_for_same_model_id`.
#[tokio::test(flavor = "current_thread")]
async fn switch_to_first_party_model_drops_minted_provider_token() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let provider = counting_provider("hall-pass", dir.path());
            let token = provider.ensure_fresh_token(None).await.rotated().unwrap();
            assert_eq!(token, "tok-1");

            let (actor, _rx) =
                make_actor_with_auth_and_credentials(None, xai_chat_state::AuthType::ApiKey, token)
                    .await;
            seed_provider_memo(&actor, provider).await;

            let model = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| c.model)
                .unwrap_or_default();

            let cfg = xai_grok_sampler::SamplerConfig {
                api_key: Some("session-jwt".to_string()),
                base_url: "https://api.x.ai/v1".to_string(),
                model,
                max_completion_tokens: None,
                temperature: None,
                top_p: None,
                api_backend: crate::sampling::ApiBackend::ChatCompletions,
                adapter_kind: Default::default(),
                request_compat: None,
                endpoint_path: None,
                auth_scheme: Default::default(),
                extra_headers: Default::default(),
                query_params: Default::default(),
                env_http_headers: Default::default(),
                context_window: 256_000,
                client_version: None,
                force_http1: false,
                max_retries: None,
                stream_tool_calls: false,
                idle_timeout_secs: None,
                client_identifier: None,
                reasoning_effort: None,
                deployment_id: None,
                user_id: None,
                origin_client: None,
                attribution_callback: None,
                bearer_resolver: None,
                supports_backend_search: false,
                compactions_remaining: None,
                compaction_at_tokens: None,
                doom_loop_recovery: None,
                header_injector: None,
                responses_codex_dialect: false,
                bedrock_request_metadata: Default::default(),
                bedrock_headers: Default::default(),
                bedrock_profile: None,
                kimi_dialect: false,
            };
            let _ = actor
                .handle_set_session_model(cfg, false, false, true, 85)
                .await;

            let creds = actor.chat_state_handle.get_credentials().await;
            assert_eq!(
                creds.api_key.as_deref(),
                Some("session-jwt"),
                "switching to a first-party model must install the session credential, \
                 not the minted provider token"
            );
        })
        .await;
}

/// Arm 4c: a 401 on a provider-backed model re-mints once and resubmits.
#[tokio::test(flavor = "current_thread")]
async fn sampler_401_on_provider_model_remints_and_resubmits() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let provider = counting_provider("test-4c-recover", dir.path());
            let token = provider.ensure_fresh_token(None).await.rotated().unwrap();
            assert_eq!(token, "tok-1");

            let (actor, _rx) =
                make_actor_with_auth_and_credentials(None, xai_chat_state::AuthType::ApiKey, token)
                    .await;
            seed_provider_memo(&actor, provider).await;
            crate::auth::test_backdate_provider_mint(
                "test-4c-recover",
                std::time::Duration::from_secs(60),
            );

            let result = actor.handle_sampling_failure(auth_error()).await;
            assert!(
                matches!(
                    result,
                    Ok(SamplerFailureRecovery::RefreshAuthAndResubmit {
                        store: RecoveredStore::AuthProvider,
                        ..
                    })
                ),
                "provider 401 must re-mint and resubmit via the provider store"
            );
            let creds = actor.chat_state_handle.get_credentials().await;
            assert_eq!(
                creds.api_key.as_deref(),
                Some("tok-2"),
                "chat-state credentials must carry the re-minted token"
            );
        })
        .await;
}

/// Arm 4c also fires for a bare 401 that did not classify as `Auth`-kind.
#[tokio::test(flavor = "current_thread")]
async fn sampler_non_auth_kind_401_on_provider_model_still_recovers() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let provider = counting_provider("test-4c-non-auth-kind", dir.path());
            let token = provider.ensure_fresh_token(None).await.rotated().unwrap();

            let (actor, _rx) =
                make_actor_with_auth_and_credentials(None, xai_chat_state::AuthType::ApiKey, token)
                    .await;
            seed_provider_memo(&actor, provider).await;
            crate::auth::test_backdate_provider_mint(
                "test-4c-non-auth-kind",
                std::time::Duration::from_secs(60),
            );

            let mut error = auth_error();
            error.kind = xai_grok_sampler::SamplingErrorKind::Api;
            let result = actor.handle_sampling_failure(error).await;
            assert!(
                matches!(
                    result,
                    Ok(SamplerFailureRecovery::RefreshAuthAndResubmit { .. })
                ),
                "a non-Auth-kind 401 on a provider model must still recover via 4c"
            );
            let creds = actor.chat_state_handle.get_credentials().await;
            assert_eq!(creds.api_key.as_deref(), Some("tok-2"));
        })
        .await;
}

/// A 401 on a request that went out with no key mints instead of
/// recovering.
#[tokio::test(flavor = "current_thread")]
async fn sampler_401_with_no_key_on_provider_model_mints_and_resubmits() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let provider = counting_provider("test-4c-no-key", dir.path());

            let (actor, _rx) = make_actor_with_auth_and_credentials(
                None,
                xai_chat_state::AuthType::ApiKey,
                "placeholder".to_string(),
            )
            .await;
            let mut creds = actor.chat_state_handle.get_credentials().await;
            creds.api_key = None;
            actor.chat_state_handle.update_credentials(creds);
            seed_provider_memo(&actor, provider).await;

            let result = actor.handle_sampling_failure(auth_error()).await;
            assert!(
                matches!(
                    result,
                    Ok(SamplerFailureRecovery::RefreshAuthAndResubmit { .. })
                ),
                "an unauthenticated 401 on a provider model must mint and resubmit"
            );
            let creds = actor.chat_state_handle.get_credentials().await;
            assert_eq!(creds.api_key.as_deref(), Some("tok-1"));
        })
        .await;
}

/// A provider model's 401 goes through the provider, never the session
/// refresher (4a/4b vs 4c exclusivity). The actor uses a session-based method,
/// so the gate would be active for a non-BYOK model; the BYOK memo is what
/// shadows it, which is the invariant under test.
#[tokio::test(flavor = "current_thread")]
async fn sampler_401_on_provider_model_never_refreshes_session() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let provider = counting_provider("test-4c-exclusive", dir.path());
            let token = provider.ensure_fresh_token(None).await.rotated().unwrap();

            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                token,
            )
            .await;
            seed_provider_memo(&actor, provider).await;
            crate::auth::test_backdate_provider_mint(
                "test-4c-exclusive",
                std::time::Duration::from_secs(60),
            );

            let result = actor.handle_sampling_failure(auth_error()).await;
            assert!(
                matches!(
                    result,
                    Ok(SamplerFailureRecovery::RefreshAuthAndResubmit { .. })
                ),
                "the provider arm must recover"
            );
            assert!(
                !called.load(Ordering::SeqCst),
                "session refresh must never fire for a provider-backed model"
            );
            let creds = actor.chat_state_handle.get_credentials().await;
            assert_eq!(creds.api_key.as_deref(), Some("tok-2"));
        })
        .await;
}

/// The pre-turn mirror of the exclusivity test: a cold cache mints the
/// provider token into chat-state, and the session refresher never fires. The
/// actor uses a session-based method, so the gate would be active for a
/// non-BYOK model; the BYOK memo is what keeps the refresher silent.
#[tokio::test(flavor = "current_thread")]
async fn pre_turn_on_provider_model_never_installs_session_token() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let provider = counting_provider("test-preturn-exclusive", dir.path());

            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "placeholder".to_string(),
            )
            .await;
            // Cold cache: no key on the wire yet.
            let mut creds = actor.chat_state_handle.get_credentials().await;
            creds.api_key = None;
            actor.chat_state_handle.update_credentials(creds);
            seed_provider_memo(&actor, provider).await;

            actor.refresh_token_if_expired().await;

            let creds = actor.chat_state_handle.get_credentials().await;
            assert_eq!(
                creds.api_key.as_deref(),
                Some("tok-1"),
                "the cold pre-turn hook must mint the provider token"
            );
            assert!(
                !called.load(Ordering::SeqCst),
                "the session refresher must never fire for a provider-backed model"
            );
        })
        .await;
}

/// A token rejected moments after mint surfaces the 401 (fresh-mint
/// guard).
#[tokio::test(flavor = "current_thread")]
async fn sampler_401_on_fresh_provider_token_surfaces_error() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let provider = counting_provider("test-4c-guard", dir.path());
            let token = provider.ensure_fresh_token(None).await.rotated().unwrap();

            let (actor, _rx) = make_actor_with_auth_and_credentials(
                None,
                xai_chat_state::AuthType::ApiKey,
                token.clone(),
            )
            .await;
            seed_provider_memo(&actor, provider).await;

            let result = actor.handle_sampling_failure(auth_error()).await;
            assert!(
                result.is_err(),
                "a fresh-minted rejected token must surface the 401, not loop"
            );
            let creds = actor.chat_state_handle.get_credentials().await;
            assert_eq!(
                creds.api_key.as_deref(),
                Some(token.as_str()),
                "credentials must be unchanged when the guard blocks the re-mint"
            );
        })
        .await;
}
