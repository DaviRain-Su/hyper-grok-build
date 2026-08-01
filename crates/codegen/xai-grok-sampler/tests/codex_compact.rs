use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use xai_grok_sampler::{SamplerConfig, SamplingClient};
use xai_grok_sampling_types::{
    ApiBackend, ConversationItem, ConversationRequest, ReasoningEffort, SamplingError, ToolSpec,
};

#[derive(Clone, Default)]
struct Capture {
    requests: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
}

async fn compact_ok(
    State(capture): State<Capture>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (HeaderMap, Json<Value>) {
    let request_number = {
        let mut requests = capture.requests.lock().expect("capture lock");
        requests.push((headers, body));
        requests.len()
    };
    let compact_type = if request_number == 1 {
        "compaction"
    } else {
        "compaction_summary"
    };
    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        "x-codex-turn-state",
        HeaderValue::from_str(&format!("compact-state-{request_number}"))
            .expect("valid turn state"),
    );
    (
        response_headers,
        Json(json!({
            "output": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "keep this request"}]
                },
                {
                    "type": compact_type,
                    "encrypted_content": "ENCRYPTED_COMPACT_STATE"
                }
            ]
        })),
    )
}

fn compact_request() -> ConversationRequest {
    ConversationRequest {
        items: vec![
            ConversationItem::system("authoritative instructions"),
            ConversationItem::user("keep this request"),
        ],
        tools: vec![ToolSpec {
            name: "read_file".into(),
            description: Some("Read a file".into()),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}}
            }),
        }],
        model: Some("gpt-test".into()),
        prompt_cache_key: Some("session-compact".into()),
        x_grok_session_id: Some("session-compact".into()),
        x_grok_turn_idx: Some("7".into()),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_compact_uses_unary_schema_and_replays_turn_state() {
    let capture = Capture::default();
    let app = Router::new()
        .route("/v1/responses/compact", post(compact_ok))
        .with_state(capture.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = SamplingClient::new(SamplerConfig {
        api_key: Some("test-key".into()),
        base_url: format!("http://{addr}/v1"),
        model: "gpt-test".into(),
        api_backend: ApiBackend::CodexResponses,
        reasoning_effort: Some(ReasoningEffort::Max),
        ..SamplerConfig::default()
    })
    .unwrap();

    for include_tools in [true, false] {
        let mut request = compact_request();
        if !include_tools {
            request.tools.clear();
        }
        let items = client
            .compact_conversation(request, "authoritative instructions".into())
            .await
            .expect("compact request succeeds")
            .expect("endpoint is supported");
        assert!(matches!(items[0], ConversationItem::User(_)));
        let ConversationItem::Compaction(compaction) = &items[1] else {
            panic!("expected compact item");
        };
        assert_eq!(compaction.id, None, "sparse relay response is accepted");
        assert_eq!(compaction.encrypted_content, "ENCRYPTED_COMPACT_STATE");
        assert!(compaction.reasoning_model_identity.is_some());
    }

    let requests = capture.requests.lock().expect("capture lock");
    assert_eq!(requests.len(), 2);
    let (first_headers, first) = &requests[0];
    assert!(first_headers.get("x-codex-turn-state").is_none());
    assert_eq!(first["model"], "gpt-test");
    assert_eq!(first["instructions"], "authoritative instructions");
    assert_eq!(first["parallel_tool_calls"], true);
    assert_eq!(first["reasoning"]["effort"], "max");
    assert_eq!(first["reasoning"]["summary"], "auto");
    assert_eq!(first["prompt_cache_key"], "session-compact");
    assert_eq!(first["text"]["verbosity"], "low");
    assert_eq!(first["tools"].as_array().map(Vec::len), Some(1));
    for forbidden in [
        "stream",
        "store",
        "tool_choice",
        "include",
        "client_metadata",
        "max_output_tokens",
    ] {
        assert!(first.get(forbidden).is_none(), "unexpected {forbidden}");
    }
    assert_eq!(
        requests[1]
            .0
            .get("x-codex-turn-state")
            .and_then(|value| value.to_str().ok()),
        Some("compact-state-1")
    );
    assert_eq!(requests[1].1["tools"], json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_compact_endpoint_is_cached_but_auth_is_not_fallback() {
    let calls = Arc::new(AtomicUsize::new(0));
    let route_calls = Arc::clone(&calls);
    let app = Router::new().route(
        "/v1/responses/compact",
        post(move || {
            let route_calls = Arc::clone(&route_calls);
            async move {
                route_calls.fetch_add(1, Ordering::SeqCst);
                (StatusCode::NOT_FOUND, Json(json!({"error": "not found"})))
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let client = SamplingClient::new(SamplerConfig {
        api_key: Some("test-key".into()),
        base_url: format!("http://{addr}/v1"),
        model: "gpt-test".into(),
        api_backend: ApiBackend::CodexResponses,
        ..SamplerConfig::default()
    })
    .unwrap();
    for _ in 0..2 {
        assert!(
            client
                .compact_conversation(compact_request(), String::new())
                .await
                .expect("unsupported is a local-fallback signal")
                .is_none()
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let auth_app = Router::new().route(
        "/v1/responses/compact",
        post(|| async {
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": {"message": "bad token"}})),
            )
        }),
    );
    let auth_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let auth_addr = auth_listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(auth_listener, auth_app).await;
    });
    let auth_client = SamplingClient::new(SamplerConfig {
        api_key: Some("test-key".into()),
        base_url: format!("http://{auth_addr}/v1"),
        model: "gpt-test".into(),
        api_backend: ApiBackend::CodexResponses,
        ..SamplerConfig::default()
    })
    .unwrap();
    let error = auth_client
        .compact_conversation(compact_request(), String::new())
        .await
        .expect_err("401 must not become a local fallback");
    assert!(matches!(
        error,
        SamplingError::Auth {
            message: _,
            credential: _
        }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_compact_model_channel_falls_back_but_generic_503_does_not() {
    let calls = Arc::new(AtomicUsize::new(0));
    let route_calls = Arc::clone(&calls);
    let app = Router::new().route(
        "/v1/responses/compact",
        post(move || {
            let route_calls = Arc::clone(&route_calls);
            async move {
                route_calls.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({
                        "error": {
                            "message": "No available channel for model gpt-test-openai-compact"
                        }
                    })),
                )
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let client = SamplingClient::new(SamplerConfig {
        api_key: Some("test-key".into()),
        base_url: format!("http://{addr}/v1"),
        model: "gpt-test".into(),
        api_backend: ApiBackend::CodexResponses,
        ..SamplerConfig::default()
    })
    .unwrap();
    for _ in 0..2 {
        assert!(
            client
                .compact_conversation(compact_request(), String::new())
                .await
                .expect("missing compact channel is a local-fallback signal")
                .is_none()
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let generic_app = Router::new().route(
        "/v1/responses/compact",
        post(|| async {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": {"message": "temporarily unavailable"}})),
            )
        }),
    );
    let generic_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let generic_addr = generic_listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(generic_listener, generic_app).await;
    });
    let generic_client = SamplingClient::new(SamplerConfig {
        api_key: Some("test-key".into()),
        base_url: format!("http://{generic_addr}/v1"),
        model: "gpt-test".into(),
        api_backend: ApiBackend::CodexResponses,
        ..SamplerConfig::default()
    })
    .unwrap();
    let error = generic_client
        .compact_conversation(compact_request(), String::new())
        .await
        .expect_err("generic 503 must remain an error");
    assert!(matches!(
        error,
        SamplingError::Api { status, .. } if status == StatusCode::SERVICE_UNAVAILABLE
    ));
}
