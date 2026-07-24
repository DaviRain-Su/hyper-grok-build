//! Integration tests for the M4 actor + request_task layer.
//!
//! Tests are integration-style (in `tests/`) rather than unit tests
//! because they require a real `tokio::runtime` and a mock HTTP
//! server (axum) to talk to the `SamplingClient`. Happy-path SSE
//! payloads come from `xai_grok_test_support::sse`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::routing::post;
use axum::{Json, Router};
use futures_util::stream::{self, StreamExt};
use indexmap::IndexMap;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

use xai_grok_sampler::{
    ApiBackend, RequestId, RetryPolicy, SamplerActor, SamplerConfig, SamplingChannel,
    SamplingErrorKind, SamplingEvent,
};
use xai_grok_sampling_types::{
    BackendToolCallItem, BackendToolKind, ConversationItem, ConversationRequest,
    DoomLoopRecoveryPolicy, ReasoningModelIdentity, UserItem,
};
use xai_grok_test_support::{SseEvent, sse};

// ---------------------------------------------------------------------------
// Mock server harness
// ---------------------------------------------------------------------------

struct MockServer {
    addr: SocketAddr,
    shutdown_tx: oneshot::Sender<()>,
}

impl MockServer {
    async fn spawn(app: Router) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });
        // Give the server a moment to start.
        tokio::time::sleep(Duration::from_millis(20)).await;
        Self { addr, shutdown_tx }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
    }
}

// ---------------------------------------------------------------------------
// Config + request helpers
// ---------------------------------------------------------------------------

fn test_config(base_url: String, model: &str) -> SamplerConfig {
    SamplerConfig {
        api_key: Some("test-key".into()),
        base_url,
        model: model.into(),
        max_completion_tokens: Some(1024),
        temperature: None,
        top_p: None,
        api_backend: ApiBackend::ChatCompletions,
        auth_scheme: Default::default(),
        extra_headers: IndexMap::new(),
        query_params: IndexMap::new(),
        env_http_headers: IndexMap::new(),
        context_window: 128_000,
        force_http1: false,
        // Keep retries minimal so tests don't take forever.
        max_retries: Some(2),
        stream_tool_calls: false,
        idle_timeout_secs: Some(30),
        reasoning_effort: None,
        origin_client: None,
        client_identifier: None,
        deployment_id: None,
        user_id: None,
        client_version: None,
        attribution_callback: None,
        bearer_resolver: None,
        supports_backend_search: false,
        compactions_remaining: None,
        compaction_at_tokens: None,
        doom_loop_recovery: None,
        header_injector: None,
        responses_codex_dialect: false,
        kimi_dialect: false,
    }
}

fn user_request(text: &str) -> ConversationRequest {
    ConversationRequest {
        items: vec![ConversationItem::User(UserItem {
            content: vec![xai_grok_sampling_types::ContentPart::Text {
                text: std::sync::Arc::<str>::from(text),
            }],
            synthetic_reason: None,
            ..Default::default()
        })],
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// SSE generators
// ---------------------------------------------------------------------------

/// Render test-helper [`SseEvent`]s (optional `event:` name + `data:`) as
/// axum SSE events for this file's router-based harness.
fn sse_events_to_axum(events: Vec<SseEvent>) -> Vec<Event> {
    events
        .into_iter()
        .map(|e| {
            let ev = Event::default().data(e.data);
            match e.event {
                Some(name) => ev.event(name),
                None => ev,
            }
        })
        .collect()
}

fn text_chunk_event(content: &str, finish: bool) -> Event {
    let chunk = json!({
        "id": "chatcmpl-test",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "delta": { "role": "assistant", "content": content },
            "finish_reason": if finish { json!("stop") } else { json!(null) }
        }]
    });
    Event::default().data(chunk.to_string())
}

// ---------------------------------------------------------------------------
// Actor lifecycle
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_then_active_count_zero_then_cancel_unknown_is_noop() {
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let cfg = test_config("http://127.0.0.1:0/v1".into(), "test-model");
    let handle = SamplerActor::spawn(cfg, RetryPolicy::default(), event_tx);
    assert_eq!(handle.active_count().await, 0);
    handle.cancel(RequestId::from("nonexistent"));
    // Re-querying should still be 0 (cancel of unknown id is no-op).
    assert_eq!(handle.active_count().await, 0);
}

// ---------------------------------------------------------------------------
// Submit + event flow
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_emits_started_first_token_channel_completed() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            let events = sse::chat_completion_events("hello world", "test-model");
            Sse::new(stream::iter(
                events.into_iter().map(Ok::<_, std::convert::Infallible>),
            ))
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let cfg = test_config(server.base_url(), "test-model");
    let handle = SamplerActor::spawn(cfg, RetryPolicy::default(), event_tx);

    let rid = RequestId::from("req-1");
    handle.submit(rid.clone(), user_request("hi"));

    let events = drain_until_terminal(&mut event_rx, Duration::from_secs(5)).await;
    server.shutdown();

    assert!(matches!(events[0], SamplingEvent::StreamStarted { .. }));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SamplingEvent::FirstToken { .. }))
    );

    let texts: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            SamplingEvent::ChannelToken {
                channel: SamplingChannel::Text,
                text,
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts.join(""), "hello world");

    match events.last().unwrap() {
        SamplingEvent::Completed {
            request_id,
            response,
            ..
        } => {
            assert_eq!(request_id, &rid);
            if let Some(a) = response.assistant() {
                assert_eq!(a.content.as_ref(), "hello world");
            } else {
                panic!("expected Assistant message");
            }
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// submit_and_collect
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_and_collect_returns_response() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            let events = sse::chat_completion_events("collected response", "test-model");
            Sse::new(stream::iter(
                events.into_iter().map(Ok::<_, std::convert::Infallible>),
            ))
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let cfg = test_config(server.base_url(), "test-model");
    let handle = SamplerActor::spawn(cfg, RetryPolicy::default(), event_tx);

    let rid = RequestId::from("req-collect");
    let result = handle
        .submit_and_collect(rid, user_request("hi"))
        .await
        .expect("collected ok");
    server.shutdown();

    let (response, _metrics) = result;
    let a = response.assistant().expect("assistant item present");
    assert_eq!(a.content.as_ref(), "collected response");
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_in_flight_request_terminates_task() {
    // Server that yields one chunk then hangs.
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            let stream = stream::iter(vec![Ok::<_, std::convert::Infallible>(text_chunk_event(
                "starting", false,
            ))])
            .chain(stream::pending());
            Sse::new(stream)
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let cfg = test_config(server.base_url(), "test-model");
    let handle = SamplerActor::spawn(cfg, RetryPolicy::default(), event_tx);

    let rid = RequestId::from("req-cancel");
    handle.submit(rid.clone(), user_request("hi"));

    // Wait for the first token to arrive so we know the request is in flight.
    let _ = await_event_matching(
        &mut event_rx,
        |e| matches!(e, SamplingEvent::FirstToken { .. }),
        Duration::from_secs(5),
    )
    .await
    .expect("first token");

    handle.cancel(rid.clone());

    // Expect a Failed event with the cancellation message.
    let failed = await_event_matching(
        &mut event_rx,
        |e| matches!(e, SamplingEvent::Failed { .. }),
        Duration::from_secs(5),
    )
    .await
    .expect("Failed event after cancel");

    if let SamplingEvent::Failed { error, .. } = failed {
        assert!(error.message.contains("cancelled"));
    }

    // Wait briefly for the task to clean up.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(handle.active_count().await, 0);
    server.shutdown();
}

// ---------------------------------------------------------------------------
// Concurrent requests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_requests_complete_with_correct_request_ids() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_handler = Arc::clone(&counter);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let counter = Arc::clone(&counter_handler);
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                let events = sse::chat_completion_events(&format!("response-{n}"), "test-model");
                Sse::new(stream::iter(
                    events.into_iter().map(Ok::<_, std::convert::Infallible>),
                ))
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let cfg = test_config(server.base_url(), "test-model");
    let handle = SamplerActor::spawn(cfg, RetryPolicy::default(), event_tx);

    let rid_a = RequestId::from("req-a");
    let rid_b = RequestId::from("req-b");
    handle.submit(rid_a.clone(), user_request("a"));
    handle.submit(rid_b.clone(), user_request("b"));

    // Drain until we see Completed for both.
    let mut completed_a = false;
    let mut completed_b = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !(completed_a && completed_b) {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            panic!(
                "timed out waiting for both requests to complete: a={completed_a}, b={completed_b}"
            );
        }
        let remaining = deadline - now;
        match tokio::time::timeout(remaining, event_rx.recv()).await {
            Ok(Some(SamplingEvent::Completed { request_id, .. })) if request_id == rid_a => {
                completed_a = true;
            }
            Ok(Some(SamplingEvent::Completed { request_id, .. })) if request_id == rid_b => {
                completed_b = true;
            }
            Ok(Some(_)) => {}
            Ok(None) => panic!("event channel closed"),
            Err(_) => panic!("timeout"),
        }
    }
    server.shutdown();
}

// ---------------------------------------------------------------------------
// Retry on transient transport error
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retries_on_500_then_succeeds() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_handler = Arc::clone(&counter);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let counter = Arc::clone(&counter_handler);
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    // First attempt: server error.
                    Err::<Sse<_>, (StatusCode, String)>((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        json!({ "error": { "message": "transient" } }).to_string(),
                    ))
                } else {
                    // Subsequent attempts: success.
                    let events = sse::chat_completion_events("ok", "test-model");
                    Ok(Sse::new(stream::iter(
                        events.into_iter().map(Ok::<_, std::convert::Infallible>),
                    )))
                }
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    // Lots of retries available; backoff is jittered around 2s on first
    // retry, so this test takes a bit to run.
    let cfg = test_config(server.base_url(), "test-model");
    let handle = SamplerActor::spawn(cfg, RetryPolicy::default(), event_tx);

    let rid = RequestId::from("req-retry");
    handle.submit(rid.clone(), user_request("hi"));

    let events = drain_until_terminal(&mut event_rx, Duration::from_secs(15)).await;
    server.shutdown();

    let saw_retrying = events
        .iter()
        .any(|e| matches!(e, SamplingEvent::Retrying { .. }));
    assert!(saw_retrying, "expected at least one Retrying event");

    match events.last().unwrap() {
        SamplingEvent::Completed { response, .. } => {
            if let Some(a) = response.assistant() {
                assert_eq!(a.content.as_ref(), "ok");
            }
        }
        other => panic!("expected Completed after retry, got {other:?}"),
    }

    assert!(
        counter.load(Ordering::SeqCst) >= 2,
        "server hit at least twice"
    );
}

// ---------------------------------------------------------------------------
// Rate limit exhausts threshold
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limit_exhausts_at_threshold_and_yields_failed() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_handler = Arc::clone(&counter);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let counter = Arc::clone(&counter_handler);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err::<
                    Sse<
                        futures_util::stream::Iter<
                            std::vec::IntoIter<Result<Event, std::convert::Infallible>>,
                        >,
                    >,
                    (StatusCode, String),
                >((
                    StatusCode::TOO_MANY_REQUESTS,
                    json!({ "error": { "message": "slow down" } }).to_string(),
                ))
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let cfg = test_config(server.base_url(), "test-model");
    let handle = SamplerActor::spawn(cfg, RetryPolicy::default(), event_tx);

    let rid = RequestId::from("req-429");
    handle.submit(rid.clone(), user_request("hi"));

    let events = drain_until_terminal(&mut event_rx, Duration::from_secs(60)).await;
    server.shutdown();

    match events.last().unwrap() {
        SamplingEvent::Failed { error, .. } => {
            assert_eq!(error.kind, SamplingErrorKind::RateLimited);
            assert_eq!(error.status_code, Some(429));
        }
        other => panic!("expected Failed(RateLimited), got {other:?}"),
    }

    let hits = counter.load(Ordering::SeqCst);
    // RATE_LIMIT_RETRY_THRESHOLD = 2, so the actor stops after two
    // attempts (the first attempt + one retry that also 429s = 2
    // hits). Allow a small slack in case scheduling fires a third
    // attempt before the threshold check.
    assert!((1..=3).contains(&hits), "expected 1-3 hits, got {hits}");
}

// ---------------------------------------------------------------------------
// Auth error -> EmitToSession (immediate)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_401_emits_failed_immediately_no_retry() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_handler = Arc::clone(&counter);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let counter = Arc::clone(&counter_handler);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err::<
                    Sse<
                        futures_util::stream::Iter<
                            std::vec::IntoIter<Result<Event, std::convert::Infallible>>,
                        >,
                    >,
                    (StatusCode, String),
                >((StatusCode::UNAUTHORIZED, "unauthorized".to_string()))
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let cfg = test_config(server.base_url(), "test-model");
    let handle = SamplerActor::spawn(cfg, RetryPolicy::default(), event_tx);

    let rid = RequestId::from("req-auth");
    handle.submit(rid.clone(), user_request("hi"));

    let events = drain_until_terminal(&mut event_rx, Duration::from_secs(5)).await;
    server.shutdown();

    // Auth errors are session-owned -- `classify_error` returns
    // `EmitToSession` so the actor emits Failed immediately without
    // retrying.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SamplingEvent::Retrying { .. }))
    );
    match events.last().unwrap() {
        SamplingEvent::Failed { error, .. } => {
            assert_eq!(error.kind, SamplingErrorKind::Auth);
        }
        other => panic!("expected Failed(Auth), got {other:?}"),
    }
    assert_eq!(counter.load(Ordering::SeqCst), 1, "no retries on 401");
}

// ---------------------------------------------------------------------------
// Anthropic Messages API: refusal stop_reason + mid-stream parse failure
// ---------------------------------------------------------------------------

fn messages_config(base_url: String) -> SamplerConfig {
    let mut cfg = test_config(base_url, "messages-compatible-model");
    cfg.api_backend = ApiBackend::Messages;
    cfg
}

/// Regression for the refusal-stop_reason incident: a well-formed stream
/// terminated by `stop_reason: "refusal"` must produce a successful
/// completion from EXACTLY ONE request — no retry storm.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn messages_refusal_stream_completes_with_single_request() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_handler = Arc::clone(&counter);
    let app = Router::new().route(
        "/v1/messages",
        post(move || {
            let counter = Arc::clone(&counter_handler);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                let events = sse::messages_api_events(
                    "I can't help with that.",
                    "messages-compatible-model",
                    "refusal",
                );
                Sse::new(stream::iter(
                    events.into_iter().map(Ok::<_, std::convert::Infallible>),
                ))
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(
        messages_config(server.base_url()),
        RetryPolicy::default(),
        event_tx,
    );

    let result = handle
        .submit_and_collect(RequestId::from("req-refusal"), user_request("hi"))
        .await;
    server.shutdown();

    let (response, _metrics) = result.expect("refusal-terminated turn must complete");
    let a = response.assistant().expect("assistant item present");
    assert_eq!(a.content.as_ref(), "I can't help with that.");
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "refusal must not trigger retries"
    );
}

/// Empty-bodied refusal: `message_start → message_delta(refusal) →
/// message_stop` with zero content blocks must complete from exactly one
/// request — the content-less response must not be classified as a retryable
/// EmptyResponse.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn messages_empty_refusal_completes_without_retry() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_handler = Arc::clone(&counter);
    let app = Router::new().route(
        "/v1/messages",
        post(move || {
            let counter = Arc::clone(&counter_handler);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                let mut events =
                    sse::messages_api_events("", "messages-compatible-model", "refusal");
                // Drop the content block events; keep start/delta/stop only.
                events.drain(1..4);
                Sse::new(stream::iter(
                    events.into_iter().map(Ok::<_, std::convert::Infallible>),
                ))
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(
        messages_config(server.base_url()),
        RetryPolicy::default(),
        event_tx,
    );

    handle.submit(RequestId::from("req-empty-refusal"), user_request("hi"));
    let events = drain_until_terminal(&mut event_rx, Duration::from_secs(10)).await;
    server.shutdown();

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SamplingEvent::Retrying { .. })),
        "content-less refusal must not be retried"
    );
    match events.last().unwrap() {
        SamplingEvent::Completed { response, .. } => {
            assert_eq!(
                response.stop_reason,
                Some(xai_grok_sampling_types::StopReason::ContentFilter)
            );
        }
        other => panic!("expected Completed, got {other:?}"),
    }
    assert_eq!(counter.load(Ordering::SeqCst), 1, "exactly one request");
}

/// A mid-stream event that fails serde (after a valid `message_start`) is a
/// deterministic response-parse failure: Fatal on the first attempt, surfaced
/// as a non-retryable Serialization error — never a retry storm.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn messages_unparseable_event_is_fatal_without_retry() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_handler = Arc::clone(&counter);
    let app =
        Router::new().route(
            "/v1/messages",
            post(move || {
                let counter = Arc::clone(&counter_handler);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let mut events =
                        sse::messages_api_events("hello", "messages-compatible-model", "end_turn");
                    // Replace the tail with a `message_delta` missing the
                    // required `delta` field — fails MessageStreamEvent serde.
                    events.truncate(4);
                    events.push(Event::default().data(
                        json!({"type":"message_delta","usage":{"output_tokens":1}}).to_string(),
                    ));
                    Sse::new(stream::iter(
                        events.into_iter().map(Ok::<_, std::convert::Infallible>),
                    ))
                }
            }),
        );
    let server = MockServer::spawn(app).await;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(
        messages_config(server.base_url()),
        RetryPolicy::default(),
        event_tx,
    );

    handle.submit(RequestId::from("req-bad-event"), user_request("hi"));
    let events = drain_until_terminal(&mut event_rx, Duration::from_secs(10)).await;
    server.shutdown();

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SamplingEvent::Retrying { .. })),
        "serde failures must not be retried"
    );
    match events.last().unwrap() {
        SamplingEvent::Failed { error, .. } => {
            assert_eq!(error.kind, SamplingErrorKind::Serialization);
            assert!(!error.is_retryable, "surfaced info must be non-retryable");
        }
        other => panic!("expected Failed(Serialization), got {other:?}"),
    }
    assert_eq!(counter.load(Ordering::SeqCst), 1, "exactly one attempt");
}

// ---------------------------------------------------------------------------
// UpdateConfig invalidates cache + applies to subsequent requests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_config_changes_subsequent_request_model() {
    use std::sync::Mutex;

    let captured_models: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_handler = Arc::clone(&captured_models);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |axum::Json(body): axum::Json<serde_json::Value>| {
            let captured = Arc::clone(&captured_handler);
            async move {
                let model = body
                    .get("model")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string();
                captured.lock().unwrap().push(model);
                let events = sse::chat_completion_events("ok", "test-model");
                Sse::new(stream::iter(
                    events.into_iter().map(Ok::<_, std::convert::Infallible>),
                ))
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let cfg = test_config(server.base_url(), "model-A");
    let handle = SamplerActor::spawn(cfg, RetryPolicy::default(), event_tx);

    let _ = handle
        .submit_and_collect(RequestId::from("req-1"), user_request("hi"))
        .await
        .expect("first req ok");

    let mut new_cfg = test_config(server.base_url(), "model-B");
    new_cfg.api_key = Some("test-key".into());
    handle.update_config(new_cfg);

    let _ = handle
        .submit_and_collect(RequestId::from("req-2"), user_request("hi"))
        .await
        .expect("second req ok");

    server.shutdown();

    let models = captured_models.lock().unwrap();
    assert_eq!(
        models.as_slice(),
        &["model-A".to_string(), "model-B".to_string()]
    );
}

// ---------------------------------------------------------------------------
// Responses doom-loop check signals
// ---------------------------------------------------------------------------

fn responses_config(base_url: String, doom_loop: Option<DoomLoopRecoveryPolicy>) -> SamplerConfig {
    let mut cfg = test_config(base_url, "test-model");
    cfg.api_backend = ApiBackend::Responses;
    cfg.doom_loop_recovery = doom_loop;
    cfg
}

/// ChatGPT/Codex may inject non-standard side-band frames into an otherwise
/// valid Responses stream (`keepalive` heartbeats and `response.metadata`).
/// Neither maps to async-openai's `ResponseStreamEvent`, so the actor must
/// skip them, preserve surrounding output, and avoid a deterministic retry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_auxiliary_frames_are_skipped_without_retry() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_handler = Arc::clone(&counter);
    let app = Router::new().route(
        "/v1/responses",
        post(move || {
            let counter = Arc::clone(&counter_handler);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                let mut events = sse::responses_api_reasoning_and_text_events(
                    "some thought",
                    "an answer",
                    "test-model",
                );
                // Wire form used by ChatGPT/Codex + async-openai's own skip:
                // `event: keepalive` with a typed JSON payload.
                events.insert(
                    1,
                    SseEvent::with_event(
                        "keepalive",
                        json!({
                            "type": "keepalive"
                        })
                        .to_string(),
                    ),
                );
                // Data-only form: no SSE event name, only the JSON type tag.
                events.insert(
                    2,
                    SseEvent::data(
                        json!({
                            "type": "keepalive",
                            "sequence_number": 1
                        })
                        .to_string(),
                    ),
                );
                events.insert(
                    3,
                    SseEvent::data(
                        json!({
                            "type": "response.metadata",
                            "sequence_number": 2,
                            "metadata": { "request_id": "req_metadata" }
                        })
                        .to_string(),
                    ),
                );
                let events = sse_events_to_axum(events);
                Sse::new(stream::iter(
                    events.into_iter().map(Ok::<_, std::convert::Infallible>),
                ))
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(
        responses_config(server.base_url(), None),
        RetryPolicy::default(),
        event_tx,
    );

    let result = handle
        .submit_and_collect(RequestId::from("req-auxiliary"), user_request("hi"))
        .await;
    server.shutdown();

    let (response, _metrics) = result.expect("auxiliary frames should not fail the turn");
    assert_eq!(response.assistant_text(), "an answer");
    assert!(
        response
            .assistant()
            .and_then(|assistant| assistant.reasoning_model_identity.as_ref())
            .is_some(),
        "actor completion must retain the sampler-authoritative route identity"
    );
    assert_eq!(counter.load(Ordering::SeqCst), 1, "must not retry");
}

/// A production actor response must carry enough provenance for its opaque
/// Responses siblings to survive a second turn on the exact same route.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_actor_two_turn_same_route_replays_all_opaque_siblings() {
    let bodies = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let bodies_handler = Arc::clone(&bodies);
    let app = Router::new().route(
        "/v1/responses",
        post(move |Json(body): Json<serde_json::Value>| {
            let bodies = Arc::clone(&bodies_handler);
            async move {
                bodies.lock().unwrap().push(body);
                let events = sse_events_to_axum(sse::responses_api_reasoning_and_text_events(
                    "private thought",
                    "portable answer",
                    // Deliberately not an alias of the submitted model. The
                    // second turn can replay reasoning only via route provenance,
                    // never through the legacy model-name fallback.
                    "provider-canonical-model",
                ));
                Sse::new(stream::iter(
                    events.into_iter().map(Ok::<_, std::convert::Infallible>),
                ))
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let expected_identity =
        ReasoningModelIdentity::new("test-model", ApiBackend::Responses, &server.base_url());
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(
        responses_config(server.base_url(), None),
        RetryPolicy::default(),
        event_tx,
    );

    let first_request_id = RequestId::from("req-route-first");
    let (mut first_response, _) = handle
        .submit_and_collect(first_request_id.clone(), user_request("first"))
        .await
        .expect("first turn completes");
    assert_eq!(
        first_response
            .assistant()
            .and_then(|assistant| assistant.model_id.as_deref()),
        Some("provider-canonical-model"),
        "fixture must not accidentally exercise the legacy model fallback"
    );
    assert_eq!(
        first_response
            .assistant()
            .and_then(|assistant| assistant.reasoning_model_identity.as_ref()),
        Some(&expected_identity),
        "submit_and_collect must return the exact sampler-authoritative route"
    );

    let completed = await_event_matching(
        &mut event_rx,
        |event| {
            matches!(
                event,
                SamplingEvent::Completed { request_id, .. }
                    if request_id == &first_request_id
            )
        },
        Duration::from_secs(5),
    )
    .await
    .expect("first turn emits a Completed event");
    let SamplingEvent::Completed { response, .. } = completed else {
        unreachable!("matching predicate only accepts Completed events")
    };
    assert_eq!(
        response
            .assistant()
            .and_then(|assistant| assistant.reasoning_model_identity.as_ref()),
        Some(&expected_identity),
        "Completed event must carry the same exact route identity"
    );

    let web_search = serde_json::from_value(json!({
        "action": {"type": "search", "query": "opaque state"},
        "id": "ws_same_route",
        "status": "completed"
    }))
    .expect("valid web-search fixture");
    let assistant_index = first_response
        .items
        .iter()
        .position(|item| matches!(item, ConversationItem::Assistant(_)))
        .expect("first response has an assistant owner");
    first_response.items.insert(
        assistant_index,
        ConversationItem::BackendToolCall(BackendToolCallItem {
            kind: BackendToolKind::WebSearch(web_search),
        }),
    );
    first_response
        .items
        .push(ConversationItem::user("follow-up"));

    handle
        .submit_and_collect(
            RequestId::from("req-route-second"),
            ConversationRequest::from_items(first_response.items),
        )
        .await
        .expect("second turn completes");
    server.shutdown();

    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2, "both turns must reach the same route once");
    let second_input = bodies[1]
        .get("input")
        .and_then(serde_json::Value::as_array)
        .expect("second request has item input");
    assert!(
        second_input.iter().any(|item| {
            item.get("type").and_then(serde_json::Value::as_str) == Some("reasoning")
        })
    );
    assert!(second_input.iter().any(|item| {
        item.get("type").and_then(serde_json::Value::as_str) == Some("web_search_call")
    }));
}

/// Server-reported doom-loop triggers flow through the actor rung onto the
/// completed response, without retries. The trigger is non-confident
/// (`@response` channel), so the recovery — which resamples only confident
/// signals — leaves it alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_doom_loop_signals_reach_completed_response() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_handler = Arc::clone(&counter);
    let app = Router::new().route(
        "/v1/responses",
        post(move || {
            let counter = Arc::clone(&counter_handler);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                let events = sse_events_to_axum(sse::responses_api_doom_loop_terminal_only_events(
                    &["tail_repetition:4@response"],
                    "some thought",
                    "an answer",
                    "test-model",
                ));
                Sse::new(stream::iter(
                    events.into_iter().map(Ok::<_, std::convert::Infallible>),
                ))
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(
        responses_config(server.base_url(), Some(DoomLoopRecoveryPolicy::default())),
        RetryPolicy::default(),
        event_tx,
    );

    let result = handle
        .submit_and_collect(RequestId::from("req-doom-signal"), user_request("hi"))
        .await;
    server.shutdown();

    let (response, _metrics) = result.expect("a signalled turn still completes");
    assert_eq!(counter.load(Ordering::SeqCst), 1, "warn-only: no resample");
    assert_eq!(response.doom_loop_signals.len(), 1);
    assert_eq!(
        response.doom_loop_signals[0].raw,
        "tail_repetition:4@response"
    );
    assert_eq!(response.assistant_text(), "an answer");
}

/// Acceptance spec for the recovery rung: a confident signal
/// (`tail_repetition:8@thinking` at the default threshold) is resampled once
/// and the clean second response is accepted, on its own budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_confident_doom_loop_signal_resamples_once() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_handler = Arc::clone(&counter);
    let app = Router::new().route(
        "/v1/responses",
        post(move || {
            let counter = Arc::clone(&counter_handler);
            async move {
                let attempt = counter.fetch_add(1, Ordering::SeqCst);
                let events = if attempt == 0 {
                    sse::responses_api_doom_loop_terminal_only_events(
                        &["tail_repetition:8@thinking"],
                        "loop loop loop",
                        "poisoned answer",
                        "test-model",
                    )
                } else {
                    sse::responses_api_reasoning_and_text_events(
                        "fresh thought",
                        "clean answer",
                        "test-model",
                    )
                };
                let events = sse_events_to_axum(events);
                Sse::new(stream::iter(
                    events.into_iter().map(Ok::<_, std::convert::Infallible>),
                ))
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(
        responses_config(server.base_url(), Some(DoomLoopRecoveryPolicy::default())),
        RetryPolicy::default(),
        event_tx,
    );

    let result = handle
        .submit_and_collect(RequestId::from("req-doom-resample"), user_request("hi"))
        .await;
    server.shutdown();

    let (response, _metrics) = result.expect("recovery accepts the clean resample");
    assert_eq!(counter.load(Ordering::SeqCst), 2, "exactly one resample");
    assert_eq!(response.assistant_text(), "clean answer");
    assert!(
        response.doom_loop_signals.is_empty(),
        "the accepted response is the clean resample"
    );
}

// ---------------------------------------------------------------------------
// Helpers for draining the event channel
// ---------------------------------------------------------------------------

/// Drain the event channel until a terminal event (`Completed` or
/// `Failed`) is received, or until `deadline` elapses.
async fn drain_until_terminal(
    rx: &mut mpsc::UnboundedReceiver<SamplingEvent>,
    timeout: Duration,
) -> Vec<SamplingEvent> {
    let mut out = Vec::new();
    let start = tokio::time::Instant::now();
    loop {
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            panic!(
                "drain_until_terminal timed out after {:?}; got {} events",
                timeout,
                out.len()
            );
        }
        let remaining = timeout - elapsed;
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ev)) => {
                let terminal = matches!(
                    ev,
                    SamplingEvent::Completed { .. } | SamplingEvent::Failed { .. }
                );
                out.push(ev);
                if terminal {
                    return out;
                }
            }
            Ok(None) => panic!("event channel closed before terminal event"),
            Err(_) => panic!(
                "drain_until_terminal timed out after {:?}; got {} events",
                timeout,
                out.len()
            ),
        }
    }
}

/// Wait for the next event matching `pred`, or return `None` on
/// timeout.
async fn await_event_matching(
    rx: &mut mpsc::UnboundedReceiver<SamplingEvent>,
    mut pred: impl FnMut(&SamplingEvent) -> bool,
    timeout: Duration,
) -> Option<SamplingEvent> {
    let start = tokio::time::Instant::now();
    loop {
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return None;
        }
        let remaining = timeout - elapsed;
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ev)) => {
                if pred(&ev) {
                    return Some(ev);
                }
            }
            Ok(None) => return None,
            Err(_) => return None,
        }
    }
}
