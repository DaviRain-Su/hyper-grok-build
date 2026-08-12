//! A session whose stored history contains an image the server rejects
//! (a 400 coded `invalid_image`) must recover within the failing turn —
//! strip the image, retry, and finish — instead of failing every turn
//! forever. The poisoned image is a *valid* PNG: client-side validation
//! passes it, so the server's coded 400 is the only line of defense.

mod acp_harness;

use acp_harness::{AutoApproveClient, RPC_TIMEOUT, connect_and_auth, prompt_turn, run_agent_test};
use agent_client_protocol::{self as acp, Agent as _};
use base64::Engine as _;
use serde_json::json;
use xai_grok_shell::sampling::{ContentPart, ConversationItem};
use xai_grok_shell::session::info::Info;
use xai_grok_shell::session::storage::{JsonlStorageAdapter, StorageAdapter};
use xai_grok_test_support::ScriptedResponse;

const SESSION_ID: &str = "poisoned-image-session";

/// A structurally valid, above-dimension-floor PNG as a base64 data URI.
fn poisoned_image_data_uri() -> String {
    let img: image::ImageBuffer<image::Rgb<u8>, Vec<u8>> =
        image::ImageBuffer::from_fn(32, 32, |x, y| image::Rgb([x as u8, y as u8, 0]));
    let mut png = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("encode fixture png");
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&png)
    )
}

/// Seed a session on disk whose history carries the poisoned image.
async fn seed_poisoned_session(cwd: &std::path::Path, image_url: &str) -> Info {
    let info = Info {
        id: acp::SessionId::new(SESSION_ID),
        cwd: cwd.to_string_lossy().into_owned(),
    };
    // Same GROK_HOME-rooted adapter the agent constructs on `session/load`.
    let storage = JsonlStorageAdapter::new();
    storage
        .init_session(&info, acp::ModelId::new("test-model"))
        .await
        .expect("init session");
    let mut user = match ConversationItem::user("what is in this image?") {
        ConversationItem::User(u) => u,
        _ => unreachable!(),
    };
    user.content.push(ContentPart::Image {
        url: std::sync::Arc::<str>::from(image_url),
    });
    storage
        .append_chat_message(&info, &ConversationItem::User(user))
        .await
        .expect("append user message");
    storage
        .append_chat_message(&info, &ConversationItem::assistant("A test pattern."))
        .await
        .expect("append assistant message");
    info
}

/// Main-turn chat-completion bodies only: side-calls (turn summary etc.) are
/// disabled via env in the harness, and excluded here by request id as a
/// second line of defense — they'd race the strict per-turn request counts.
fn chat_completion_bodies(server: &xai_grok_test_support::MockInferenceServer) -> Vec<String> {
    server
        .requests()
        .into_iter()
        .filter(|r| r.path == "/v1/chat/completions")
        .filter(|r| {
            !r.header("x-grok-req-id")
                .is_some_and(|id| id.starts_with("xai-turn-summary-"))
        })
        .map(|r| r.body.map(|b| b.to_string()).unwrap_or_default())
        .collect()
}

#[test]
fn poisoned_image_session_recovers_within_the_failing_turn() {
    run_agent_test(|cwd, server| async move {
        let image_url = poisoned_image_data_uri();
        // Distinctive payload substring to track the image through request bodies.
        let image_marker = &image_url[image_url.len() - 48..];
        let info = seed_poisoned_session(&cwd, &image_url).await;

        let (conn, _init) = connect_and_auth(AutoApproveClient, "test-client").await;
        tokio::time::timeout(
            RPC_TIMEOUT,
            conn.load_session(acp::LoadSessionRequest::new(
                info.id.clone(),
                cwd.to_path_buf(),
            )),
        )
        .await
        .expect("session/load timed out")
        .expect("session/load failed");

        // First attempt of the next turn: the server rejects the image with
        // the flat coded envelope the real server emits. The scripted
        // response is one-shot; the strip-retry falls through to echo mode
        // (= server accepts the cleaned request).
        server.enqueue_response(
            "/v1/chat/completions",
            ScriptedResponse::json(
                400,
                json!({
                    "code": "invalid_image",
                    "error": "Base64 string of provided image cannot be decoded.",
                }),
            ),
        );

        // Before the coded detection this turn was Fatal: no strip-retry
        // happened and the prompt failed.
        prompt_turn(&conn, &info.id, "hi").await;

        let bodies = chat_completion_bodies(&server);
        assert!(
            bodies.len() >= 2,
            "expected the rejected attempt plus a strip-retry, saw {} request(s)",
            bodies.len()
        );
        assert!(
            bodies[0].contains(image_marker),
            "first attempt must carry the poisoned image"
        );
        let retry = &bodies[bodies.len() - 1];
        assert!(
            !retry.contains(image_marker),
            "strip-retry must not resend the poisoned image"
        );
        assert!(
            retry.contains("[image removed"),
            "strip-retry must carry the placeholder so the model knows an image was there"
        );
    });
}
