//! Redirect isolation for product/session `x-grok-*` headers (integration).
//!
//! Production sampling clients use a custom policy that follows only
//! **HTTPS same-origin** redirects. Cross-origin and HTTPS→HTTP hops stop so
//! custom headers are never forwarded to a third party (reqwest does not strip
//! `x-grok-*` on cross-origin redirects — proven by the control experiment).
//!
//! These tests use **plain HTTP localhost** listeners to prove **cross-origin
//! stop** (different ports = different origins) for both default H2 and
//! `force_http1` clients. They do **not** claim a real TLS/H2 same-origin
//! wire follow; pure HTTPS/same-origin/hop decision unit tests live in
//! `shared_http` module tests.

mod support;

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use tokio::net::TcpListener;
use xai_grok_sampler::SamplingClient;
use xai_grok_sampling_types::ApiBackend;

type Captured = Arc<Mutex<Vec<HeaderMap>>>;

async fn spawn_capture(captured: Captured, path: &str) -> (String, u16) {
    let path = path.to_string();
    let app = Router::new().route(
        &path,
        post(move |headers: HeaderMap| {
            let captured = Arc::clone(&captured);
            async move {
                captured.lock().unwrap().push(headers);
                (
                    StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    r#"{"choices":[]}"#,
                )
                    .into_response()
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), addr.port())
}

async fn spawn_redirector(location: String, path: &str) -> String {
    let path = path.to_string();
    let app = Router::new().route(
        &path,
        post(move || {
            let location = location.clone();
            async move {
                (
                    StatusCode::TEMPORARY_REDIRECT,
                    [(axum::http::header::LOCATION, location)],
                )
                    .into_response()
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// Same-origin path redirect: a following client with the production policy
/// shape would follow HTTPS same-origin; on plain HTTP localhost the policy
/// **stops** (HTTPS required). This test documents that HTTP same-origin is
/// not followed — production endpoints are HTTPS.
///
/// Cross-origin: two ports; third party must receive zero requests when using
/// the production shared client (stops cross-origin).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_origin_redirect_is_not_followed_by_shared_client() {
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let (third, _) = spawn_capture(Arc::clone(&captured), "/v1/chat/completions").await;
    let third_path = format!("{third}/v1/chat/completions");
    let first = spawn_redirector(third_path, "/v1/chat/completions").await;

    let mut cfg = support::test_config(&format!("{first}/v1"), "test-key");
    cfg.api_backend = ApiBackend::ChatCompletions;
    cfg.extra_headers
        .insert("x-grok-session-id".into(), "sess-secret".into());
    cfg.extra_headers
        .insert("x-grok-client-identifier".into(), "cli-secret".into());
    let client = SamplingClient::new(cfg).expect("client builds");
    support::send_one(&client).await;

    let hits = captured.lock().unwrap().clone();
    assert!(
        hits.is_empty(),
        "shared client must not follow cross-origin redirect; third-party got {} request(s)",
        hits.len()
    );
}

/// force_http1 path uses the same redirect policy constructor (cross-origin stop).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_origin_redirect_not_followed_with_force_http1() {
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let (third, _) = spawn_capture(Arc::clone(&captured), "/v1/chat/completions").await;
    let third_path = format!("{third}/v1/chat/completions");
    let first = spawn_redirector(third_path, "/v1/chat/completions").await;

    let mut cfg = support::test_config(&format!("{first}/v1"), "test-key");
    cfg.api_backend = ApiBackend::ChatCompletions;
    cfg.force_http1 = true;
    cfg.extra_headers
        .insert("x-grok-session-id".into(), "sess-secret".into());
    let client = SamplingClient::new(cfg).expect("client builds");
    support::send_one(&client).await;

    let hits = captured.lock().unwrap().clone();
    assert!(
        hits.is_empty(),
        "http1 client must not follow cross-origin redirect; got {} hit(s)",
        hits.len()
    );
}

/// Same-origin path redirect on a single listener is followed by a client that
/// uses limited redirects (control for "same origin works"). Production policy
/// additionally requires HTTPS, so we use a custom limited policy here to show
/// same-origin path hops succeed when scheme matches.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_origin_path_redirect_is_followed_when_policy_allows() {
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let path_final = "/v1/chat/completions";
    let path_redir = "/v1/redirect";

    // Single server: redirect path → final path (same origin).
    let cap = Arc::clone(&captured);
    let app = Router::new()
        .route(
            path_final,
            post(move |headers: HeaderMap| {
                let cap = Arc::clone(&cap);
                async move {
                    cap.lock().unwrap().push(headers);
                    (
                        StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        r#"{"choices":[]}"#,
                    )
                        .into_response()
                }
            }),
        )
        .route(
            path_redir,
            post(|| async {
                (
                    StatusCode::TEMPORARY_REDIRECT,
                    [(
                        axum::http::header::LOCATION,
                        "/v1/chat/completions".to_string(),
                    )],
                )
                    .into_response()
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // Limited policy (not production HTTPS filter) proves same-origin follow.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .unwrap();
    let url = format!("http://{addr}{path_redir}");
    let resp = client
        .post(&url)
        .header("x-test", "1")
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("same-origin follow");
    assert!(resp.status().is_success(), "status {}", resp.status());
    let hits = captured.lock().unwrap().clone();
    assert_eq!(hits.len(), 1, "same-origin path redirect should hit final");
}

/// Control: a fully following client forwards `x-grok-*` cross-origin —
/// why production must stop cross-origin (and prefer HTTPS same-origin only).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn following_client_forwards_x_grok_on_cross_origin_redirect_proving_policy() {
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let (third, _) = spawn_capture(Arc::clone(&captured), "/v1/chat/completions").await;
    let third_path = format!("{third}/v1/chat/completions");
    let first = spawn_redirector(third_path, "/v1/chat/completions").await;

    let following = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .unwrap();

    let url = format!("{first}/v1/chat/completions");
    let resp = following
        .post(&url)
        .header("x-grok-session-id", "sess-leak")
        .header("x-grok-client-identifier", "cli-leak")
        .header(reqwest::header::AUTHORIZATION, "Bearer test-key")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&serde_json::json!({"model": "m", "messages": []}))
        .send()
        .await
        .expect("following client completes");
    assert!(
        resp.status().is_success(),
        "following client should land on third-party 200, got {}",
        resp.status()
    );

    let hits = captured.lock().unwrap().clone();
    assert_eq!(hits.len(), 1, "following client must reach third-party");
    let h = &hits[0];
    assert_eq!(
        h.get("x-grok-session-id").and_then(|v| v.to_str().ok()),
        Some("sess-leak"),
        "reqwest does NOT strip custom x-grok-* on cross-origin redirect"
    );
}
