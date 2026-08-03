//! Real-turn-loop tests against a mock server that 401s unauthenticated
//! requests and 200s a fresh bearer: a fail-closed (credential-less) 401
//! must not consume `AuthRetrySchedule` budget — the field failure mode
//! where each sleep cycle burned one slot — while credentialed 401s must
//! still exhaust after `MAX_RETRIES`.

use super::support::*;
use super::*;
use crate::auth::{AuthManager, AuthMode, GrokAuth, GrokComConfig};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use xai_grok_test_support::{MockInferenceServer, MockModelEntry};

/// The token the mock server accepts and the refresher mints on success.
const FRESH_TOKEN: &str = "refreshed-test-token";

/// With `fail_pre_request`, mimics the post-wake sequence: pre-send
/// (`PreRequest`) refreshes fail transiently so the send goes out
/// fail-closed, while the 401-triggered recovery (`ServerRejected`)
/// succeeds and mints [`FRESH_TOKEN`]. Otherwise always succeeds.
struct WakeGapRefresher {
    calls: Arc<AtomicU32>,
    fail_pre_request: bool,
}

#[async_trait::async_trait]
impl crate::auth::refresh::TokenRefresher for WakeGapRefresher {
    async fn refresh(
        &self,
        reason: crate::auth::refresh::RefreshReason,
    ) -> crate::auth::refresh::RefreshOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_pre_request && reason == crate::auth::refresh::RefreshReason::PreRequest {
            return crate::auth::refresh::RefreshOutcome::TransientFailure {
                message: "simulated post-wake network gap".to_string(),
            };
        }
        crate::auth::refresh::RefreshOutcome::success(GrokAuth {
            key: FRESH_TOKEN.to_string(),
            auth_mode: AuthMode::Oidc,
            refresh_token: Some("rt-new".into()),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            ..GrokAuth::test_default()
        })
    }
}

/// `(tempdir, manager)` with a hard-expired OIDC token, so the wire-valid
/// resolver has nothing to stamp until the refresher succeeds. The tempdir
/// must outlive the manager (auth.json path).
fn expired_auth_manager(
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

/// `x.ai/session_notification` payloads the client was sent.
type XaiUpdates = Arc<parking_lot::Mutex<Vec<serde_json::Value>>>;

fn drain_gateway(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
) -> XaiUpdates {
    let captured = XaiUpdates::default();
    let sink = captured.clone();
    tokio::task::spawn_local(async move {
        while let Some(msg) = rx.recv().await {
            match msg {
                xai_acp_lib::AcpClientMessage::SessionNotification(args) => {
                    let _ = args.response_tx.send(Ok(()));
                }
                xai_acp_lib::AcpClientMessage::ExtNotification(args) => {
                    if let Ok(value) = serde_json::from_str(args.params.get()) {
                        sink.lock().push(value);
                    }
                }
                _ => {}
            }
        }
    });
    captured
}

/// `(error_type, message)` of the turn's terminal `retryState`, if the client
/// was told about one at all.
fn terminal_failure(updates: &XaiUpdates) -> Option<(String, String)> {
    updates.lock().iter().find_map(|value| {
        let update = value.get("update")?;
        if update.get("sessionUpdate")? != "retry_state" || update.get("type")? != "failed" {
            return None;
        }
        Some((
            update.get("error_type")?.as_str()?.to_owned(),
            update.get("message")?.as_str()?.to_owned(),
        ))
    })
}

fn drain_persistence(mut rx: tokio::sync::mpsc::UnboundedReceiver<PersistenceMsg>) {
    tokio::task::spawn_local(async move {
        while let Some(msg) = rx.recv().await {
            if let PersistenceMsg::FlushAndAck { respond_to } = msg {
                let _ = respond_to.send(());
            }
        }
    });
}

/// Actor wired for session-token auth against the mock server: real sampler,
/// `cached_token` method, `NotByok` model facts (so the session-token gate is
/// active against the loopback URL), and the supplied auth manager.
async fn session_token_actor(
    server: &MockInferenceServer,
    auth_manager: Arc<AuthManager>,
) -> (Arc<SessionActor>, XaiUpdates) {
    let sampling_cfg = xai_grok_sampler::SamplerConfig {
        base_url: server.url(),
        model: "test".to_string(),
        api_backend: xai_grok_sampler::ApiBackend::Responses,
        context_window: 256_000,
        max_retries: Some(0),
        idle_timeout_secs: Some(30),
        ..Default::default()
    };
    let (sampler_event_tx, sampler_event_rx) =
        tokio::sync::mpsc::unbounded_channel::<xai_grok_sampler::SamplingEvent>();
    let sampler_handle = xai_grok_sampler::SamplerActor::spawn(
        sampling_cfg,
        xai_grok_sampler::RetryPolicy {
            max_retries: 0,
            rate_limit_retry_threshold: 0,
            ..Default::default()
        },
        sampler_event_tx,
    );

    let (gateway_tx, gateway_rx) = tokio::sync::mpsc::unbounded_channel();
    let xai_updates = drain_gateway(gateway_rx);
    let (persistence_tx, persistence_rx) = tokio::sync::mpsc::unbounded_channel();
    drain_persistence(persistence_rx);

    let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    actor.sampler_handle = sampler_handle;
    actor.auth_manager = Some(auth_manager);
    actor.auth_method_id = test_auth_method_id("cached_token");

    let mut cfg = actor
        .chat_state_handle
        .get_sampling_config()
        .await
        .expect("test actor has sampling config");
    cfg.base_url = server.url();
    cfg.api_backend = xai_grok_sampling_types::ApiBackend::Responses;
    cfg.model = "test".to_string();
    actor.chat_state_handle.update_sampling_config(cfg);
    let mut creds = actor.chat_state_handle.get_credentials().await;
    creds.api_key = None;
    creds.auth_type = xai_chat_state::AuthType::SessionToken;
    actor.chat_state_handle.update_credentials(creds);

    // Definite NotByok: the session-token gate must stay active against the
    // loopback mock URL (an `Unknown` would demand a first-party host).
    actor
        .model_auth_memo
        .replace(Some(crate::session::acp_session::ModelAuthMemo {
            model_id: "test".to_string(),
            facts: crate::agent::config::ModelAuthFacts {
                byok: crate::agent::auth_method::ModelByok::NotByok,
                auth_scheme: Default::default(),
                oauth_platform: None,
                platform_oauth_active: false,
            },
            provider: None,
            catalog_epoch: 0,
        }));

    actor
        .workspace_ops
        .bind_local_session(
            &actor.session_id_string(),
            actor.tool_context.cwd.as_path().to_path_buf(),
            actor.tool_context.hunk_tracker_handle.clone(),
            actor.agent.borrow().tool_bridge().toolset(),
            None,
        )
        .expect("bind_local_session");

    let actor = Arc::new(actor);
    {
        let drainer = actor.clone();
        let mut sampler_event_rx = sampler_event_rx;
        tokio::task::spawn_local(async move {
            while let Some(event) = sampler_event_rx.recv().await {
                drainer.handle_sampling_event(event).await;
            }
        });
    }
    (actor, xai_updates)
}

async fn run_prompt(
    actor: &Arc<SessionActor>,
    prompt_id: &str,
) -> Result<crate::session::commands::PromptTurnOk, acp::Error> {
    let prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(
        "hello".to_string(),
    ))];
    // Pin from a thin never-inlined frame (production `run_task` path).
    // No stacker / RUST_MIN_STACK — see docs/test-isolation.md §8.3.
    let fut = crate::session::acp_session::box_turn_future(|| {
        actor.handle_prompt(
            prompt_id,
            prompt_blocks,
            PromptMode::Agent,
            None,
            None,
            None,
            None,
            true,
            None,
            None,
            None,
        )
    });
    tokio::time::timeout(Duration::from_secs(60), fut)
        .await
        .expect("turn must finish within timeout")
}

/// The wake sequence: the resolver has nothing wire-valid, the send goes
/// out with no `Authorization` header, the server 401s it, recovery lands a
/// fresh token. The turn must survive and resubmit with the fresh bearer.
#[tokio::test(flavor = "current_thread")]
async fn fail_closed_401_is_uncharged_and_turn_survives() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let server = MockInferenceServer::start_with_required_auth(
                vec![MockModelEntry::new("test")],
                FRESH_TOKEN,
            )
            .await
            .expect("mock inference server");

            let calls = Arc::new(AtomicU32::new(0));
            // Pre-send refreshes fail like a post-wake network gap, so the
            // first send goes out fail-closed; the 401-recovery refresh
            // succeeds.
            let refresher = Arc::new(WakeGapRefresher {
                calls: calls.clone(),
                fail_pre_request: true,
            });
            let (_dir, am) = expired_auth_manager(refresher);
            let (actor, _updates) = session_token_actor(&server, am).await;

            let outcome = run_prompt(&actor, "auth-retry-budget-fail-closed").await;
            assert!(
                outcome.is_ok(),
                "fail-closed 401 must not fail the turn: {outcome:?}"
            );

            let inference: Vec<_> = server
                .requests()
                .into_iter()
                .filter(|r| r.path.contains("/responses"))
                .collect();
            assert!(
                inference.len() >= 2,
                "expected the fail-closed send plus the resubmit; got {}",
                inference.len()
            );
            assert_eq!(
                inference[0].authorization, None,
                "first send must carry no Authorization header"
            );
            assert_eq!(
                inference.last().unwrap().authorization.as_deref(),
                Some(&format!("Bearer {FRESH_TOKEN}") as &str),
                "resubmit must carry the freshly refreshed bearer"
            );
            assert!(
                calls.load(Ordering::SeqCst) >= 2,
                "both the failing pre-flight and the recovery refresh must run"
            );
        })
        .await;
}

/// Real credential rejections must still terminate: when every request
/// carries a bearer the server rejects, the escalating budget exhausts after
/// `MAX_RETRIES` and the failure names authenticated rejections — not a
/// generic budget message. `start_paused` auto-advances the backoff ladder.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn authenticated_401s_still_exhaust_after_three_retries() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // The server only accepts a token the refresher never mints, so
            // every authenticated send is rejected.
            let server = MockInferenceServer::start_with_required_auth(
                vec![MockModelEntry::new("test")],
                "never-issued-token",
            )
            .await
            .expect("mock inference server");

            let refresher = Arc::new(WakeGapRefresher {
                calls: Arc::new(AtomicU32::new(0)),
                fail_pre_request: false,
            });
            let (_dir, am) = expired_auth_manager(refresher);
            let (actor, updates) = session_token_actor(&server, am).await;

            let outcome = run_prompt(&actor, "auth-retry-budget-exhaust").await;
            let err = outcome.expect_err("authenticated 401s must exhaust and fail the turn");
            let rendered = serde_json::to_string(&err.data).unwrap_or_default();
            assert!(
                rendered.contains("authenticated inference requests were still rejected"),
                "exhaustion must name authenticated rejections, got: {rendered}"
            );

            let authenticated = server
                .requests()
                .into_iter()
                .filter(|r| r.path.contains("/responses"))
                .filter(|r| r.authorization.as_deref() == Some(&format!("Bearer {FRESH_TOKEN}")))
                .count();
            assert_eq!(
                authenticated, 4,
                "initial send plus MAX_RETRIES resubmits, all authenticated"
            );

            // The budget is the one terminal path that lives outside
            // `handle_sampling_failure`, and it used to return its error with
            // no notification at all — leaving the pager with no re-auth
            // prompt and no turn-failed block for a turn that died on 401s.
            let (error_type, message) = terminal_failure(&updates)
                .expect("an exhausted turn must report a terminal retryState");
            assert_eq!(
                error_type, "auth",
                "the client keys its re-auth prompt off this: {message}"
            );
            assert!(
                message.contains("authenticated inference requests were still rejected"),
                "the notification must carry the same story as the error: {message}"
            );
        })
        .await;
}

/// Explicit 2 MiB named-thread + current-thread runtime + LocalSet.
/// Does **not** rely on libtest's default stack size (which varies by
/// harness / OS). Debug builds must pass here; release can be enabled in
/// CI workflow separately.
#[test]
fn auth_retry_budget_fail_closed_on_explicit_2mib_stack() {
    run_on_2mib_thread("auth-retry-2mib-fail-closed", || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let local = tokio::task::LocalSet::new();
            local
                .run_until(async {
                    let server = MockInferenceServer::start_with_required_auth(
                        vec![MockModelEntry::new("test")],
                        FRESH_TOKEN,
                    )
                    .await
                    .expect("mock inference server");
                    let calls = Arc::new(AtomicU32::new(0));
                    let refresher = Arc::new(WakeGapRefresher {
                        calls: calls.clone(),
                        fail_pre_request: true,
                    });
                    let (_dir, am) = expired_auth_manager(refresher);
                    let (actor, _xai_updates) = session_token_actor(&server, am).await;
                    let outcome = run_prompt(&actor, "auth-retry-2mib-fail-closed").await;
                    assert!(
                        outcome.is_ok(),
                        "fail-closed 401 must not fail the turn on 2MiB stack: {outcome:?}"
                    );
                    assert!(
                        calls.load(Ordering::SeqCst) >= 2,
                        "pre-flight + recovery refresh must run"
                    );
                })
                .await;
        });
    });
}

/// Same explicit-stack harness for the authenticated budget-exhaust path
/// (virtual time via start_paused is unavailable outside #[tokio::test];
/// we still exercise construction + first poll of the large turn future).
#[test]
fn auth_retry_budget_constructs_turn_on_explicit_2mib_stack() {
    run_on_2mib_thread("auth-retry-2mib-construct", || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let local = tokio::task::LocalSet::new();
            local
                .run_until(async {
                    let server = MockInferenceServer::start_with_required_auth(
                        vec![MockModelEntry::new("test")],
                        "never-issued-token",
                    )
                    .await
                    .expect("mock");
                    let refresher = Arc::new(WakeGapRefresher {
                        calls: Arc::new(AtomicU32::new(0)),
                        fail_pre_request: false,
                    });
                    let (_dir, am) = expired_auth_manager(refresher);
                    let (actor, _xai_updates) = session_token_actor(&server, am).await;
                    // Construct + start the large future; timeout quickly if
                    // the budget path would sleep forever without paused time.
                    let prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(
                        "hello".to_string(),
                    ))];
                    let fut = crate::session::acp_session::box_turn_future(|| {
                        actor.handle_prompt(
                            "auth-retry-2mib-construct",
                            prompt_blocks,
                            PromptMode::Agent,
                            None,
                            None,
                            None,
                            None,
                            true,
                            None,
                            None,
                            None,
                        )
                    });
                    // Even if the turn later fails/times out, constructing and
                    // polling the future on a 2 MiB stack must not SIGABRT.
                    let _ = tokio::time::timeout(Duration::from_millis(500), fut).await;
                })
                .await;
        });
    });
}

fn run_on_2mib_thread(name: &str, f: impl FnOnce() + Send + 'static) {
    const TWO_MIB: usize = 2 * 1024 * 1024;
    let builder = std::thread::Builder::new()
        .name(name.into())
        .stack_size(TWO_MIB);
    let handle = builder.spawn(f).expect("spawn 2MiB thread");
    handle.join().expect("2MiB thread panicked or overflowed");
}
