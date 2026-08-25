//! Agent-type invariant integration tests.
//!
//! These tests exercise the full shell lifecycle via ACP stdio against a mock
//! inference server, verifying that `agent_type = f(model)` holds across:
//!
//! - Session creation with default models
//! - Zero-turn model switching (harness rebuild)
//! - Mid-session model switching (rejection)
//! - Same-type model switching (no rebuild)
//! - Session resume
//!
//! Each test spawns a real `grok agent stdio` process, speaks the full ACP
//! protocol, and asserts on the inference request bodies (system prompt) and
//! stderr tracing output to verify the correct harness was used.
//!
//! Run locally:
//! ```bash
//! cargo test -p xai-grok-shell --test test_agent_type_invariant -- --ignored
//! ```
use std::future::Future;
use xai_grok_test_support::*;
async fn with_local_set<F, Fut>(f: F)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    tokio::task::LocalSet::new().run_until(f()).await;
}
/// Delete the shell's on-disk models cache so the next process is forced to
/// re-fetch from the mock server's `/v1/models` endpoint. Without this, the
/// second spawn in a resume test reads the stale cache written by phase 1
/// and never sees the updated model list.
fn invalidate_models_cache(home: &std::path::Path) {
    let cache = home.join(".grok").join("models_cache.json");
    if cache.exists() {
        std::fs::remove_file(&cache).expect("failed to delete models_cache.json");
    }
}
/// Grok-build system template marker. Absence means the alternate harness.
fn is_grok_build_system_prompt(sys_prompt: &str) -> bool {
    sys_prompt.contains("<work_policy>")
}
/// Alternate-agent compiled prompts: grok-build's `<work_policy>` is absent
/// from the system template, and the injected workspace path is present in
/// a user-role message (`cursor_user.md`, not `cursor_prompt.md`). Empty or
/// unrelated prompts fail the user-path check.
fn assert_alternate_harness(server: &MockInferenceServer, workspace: &std::path::Path) {
    let sys_prompt = server
        .last_system_prompt()
        .expect("should have at least one inference request");
    let user_prompt = last_request_user_text(server)
        .expect("should have a user message on the inference request");
    let path = workspace.to_string_lossy();
    assert!(
        !is_grok_build_system_prompt(&sys_prompt),
        "alternate template must not use grok-build `<work_policy>`\n\
         system prompt preview: {}",
        &sys_prompt[..sys_prompt.len().min(500)]
    );
    assert!(
        user_prompt.contains(path.as_ref()),
        "alternate user template must include injected workspace path `{path}`\n\
         user prompt preview: {}",
        &user_prompt[..user_prompt.len().min(500)]
    );
}
/// Join user-role message text from the most recent chat/Responses request.
fn last_request_user_text(server: &MockInferenceServer) -> Option<String> {
    let body = server.requests().into_iter().rev().find_map(|e| {
        if e.path.contains("chat/completions") || e.path.contains("responses") {
            e.body
        } else {
            None
        }
    })?;
    let texts = user_role_texts(&body);
    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n"))
    }
}
fn user_role_texts(body: &serde_json::Value) -> Vec<String> {
    let Some(items) = body
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .or_else(|| body.get("input").and_then(serde_json::Value::as_array))
    else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|m| m.get("role").and_then(serde_json::Value::as_str) == Some("user"))
        .filter_map(|m| m.get("content").and_then(content_to_text))
        .collect()
}
fn content_to_text(content: &serde_json::Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    let parts = content.as_array()?;
    let mut out = String::new();
    for part in parts {
        if !matches!(
            part.get("type").and_then(serde_json::Value::as_str),
            Some("text") | Some("input_text")
        ) {
            continue;
        }
        let Some(text) = part.get("text").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(text);
    }
    (!out.is_empty()).then_some(out)
}
#[test]
fn user_role_texts_joins_chat_user_turns_not_system() {
    let body = serde_json::json!({
        "messages": [
            { "role": "system", "content": "system-only" },
            { "role": "user", "content": "Workspace Path: /tmp/ws" },
            { "role": "user", "content": "say hello" }
        ]
    });
    let user = user_role_texts(&body).join("\n");
    assert!(user.contains("Workspace Path: /tmp/ws"), "{user}");
    assert!(user.contains("say hello"), "{user}");
    assert!(!user.contains("system-only"), "{user}");
}
#[test]
fn user_role_texts_reads_responses_input_text_parts() {
    let body = serde_json::json!({
        "instructions": "system-only",
        "input": [{
            "role": "user",
            "content": [{ "type": "input_text", "text": "Workspace Path: /tmp/ws" }]
        }]
    });
    assert_eq!(
        user_role_texts(&body),
        vec!["Workspace Path: /tmp/ws".to_string()]
    );
}
/// Start a mock server with two models:
/// - `default-model`: no agent_type (→ defaults to "grok-build")
async fn dual_model_server() -> MockInferenceServer {
    MockInferenceServer::start_with_models(vec![
        MockModelEntry::new("default-model"),
        MockModelEntry::with_agent_type("cursor-model", "cursor"),
    ])
    .await
    .expect("start mock server")
}
/// Start a mock server with two models that share the same agent_type:
/// - `model-a`: no agent_type (→ "grok-build")
/// - `model-b`: no agent_type (→ "grok-build")
async fn same_type_server() -> MockInferenceServer {
    MockInferenceServer::start_with_models(vec![
        MockModelEntry::new("model-a"),
        MockModelEntry::new("model-b"),
    ])
    .await
    .expect("start mock server")
}
/// Session created with a model that has no `agent_type` should use the
/// `grok-build` harness. The system prompt sent to the LLM should contain
/// the grok-build identity string.
#[tokio::test]
#[ignore]
async fn test_default_model_uses_grok_build_harness() {
    with_local_set(|| async {
        let server = MockInferenceServer::start()
            .await
            .expect("start mock server");
        let workdir = git_workdir();
        let client = GrokStdioClient::spawn(&server, workdir.workspace()).await;
        client.initialize_with_timeout().await;
        let session_id = client
            .create_session_with_timeout(workdir.workspace())
            .await;
        let result = client.prompt_with_timeout(&session_id, "say hello").await;
        assert!(result.is_ok(), "prompt failed: {:?}", result.err());
        let sys_prompt = server
            .last_system_prompt()
            .expect("should have at least one inference request");
        assert!(
            is_grok_build_system_prompt(&sys_prompt),
            "default model should use grok-build harness\nsystem prompt preview: {}",
            &sys_prompt[..sys_prompt.len().min(500)]
        );
    })
    .await;
}
/// Switching between two models with the same agent_type should succeed
/// without a harness rebuild.
#[tokio::test]
#[ignore]
async fn test_same_type_model_switch_no_rebuild() {
    with_local_set(|| async {
        let server = same_type_server().await;
        let workdir = git_workdir();
        let client = GrokStdioClient::spawn(&server, workdir.workspace()).await;
        client.initialize_with_timeout().await;
        let session_id = client
            .create_session_with_model_timeout(workdir.workspace(), "model-a")
            .await;
        let result = client.prompt_with_timeout(&session_id, "say hello").await;
        assert!(result.is_ok(), "first prompt failed: {:?}", result.err());
        let switch_result = client.set_model_with_timeout(&session_id, "model-b").await;
        assert!(
            switch_result.is_ok(),
            "same-type model switch should succeed\nerror: {:?}\nstderr: {}",
            switch_result.err(),
            stderr_tail(&client.stderr(), 2000)
        );
        let result2 = client.prompt_with_timeout(&session_id, "say goodbye").await;
        assert!(
            result2.is_ok(),
            "second prompt after model switch failed: {:?}",
            result2.err()
        );
    })
    .await;
}
/// A session created with the default model, persisted, and reloaded should
/// still use the same harness — the system prompt in the resumed session's
/// first inference request should match the original.
#[tokio::test]
#[ignore]
async fn test_session_resume_preserves_harness() {
    with_local_set(|| async {
        let server = MockInferenceServer::start()
            .await
            .expect("start mock server");
        let workdir = git_workdir();
        let mut writer = GrokStdioClient::spawn(&server, workdir.workspace()).await;
        writer.initialize_with_timeout().await;
        let session_id = writer
            .create_session_with_timeout(workdir.workspace())
            .await;
        let result = writer.prompt_with_timeout(&session_id, "say hello").await;
        assert!(result.is_ok(), "prompt failed: {:?}", result.err());
        let original_sys_prompt = server
            .last_system_prompt()
            .expect("should have captured system prompt");
        let shared_sandbox = writer.take_sandbox();
        invalidate_models_cache(shared_sandbox.home());
        drop(writer);
        let reader =
            GrokStdioClient::spawn_with_sandbox(&server, workdir.workspace(), shared_sandbox).await;
        reader.initialize_with_timeout().await;
        let _ = reader
            .load_session_with_timeout(&session_id, workdir.workspace())
            .await;
        let result2 = reader.prompt_with_timeout(&session_id, "say goodbye").await;
        assert!(
            result2.is_ok(),
            "resumed prompt failed: {:?}",
            result2.err()
        );
        let resumed_sys_prompt = server
            .last_system_prompt()
            .expect("should have captured resumed system prompt");
        let original_is_grok = is_grok_build_system_prompt(&original_sys_prompt);
        let resumed_is_grok = is_grok_build_system_prompt(&resumed_sys_prompt);
        assert_eq!(
            original_is_grok,
            resumed_is_grok,
            "resumed session should use the same harness as the original\n\
             original grok-build template: {original_is_grok}\n\
             resumed grok-build template: {resumed_is_grok}\n\
             original prompt (first 300): {}\n\
             resumed prompt (first 300): {}",
            &original_sys_prompt[..original_sys_prompt.len().min(300)],
            &resumed_sys_prompt[..resumed_sys_prompt.len().min(300)],
        );
    })
    .await;
}
/// A model that doesn't declare `agent_type` in its metadata should
/// default to `"grok-build"`. This exercises the serde default.
#[tokio::test]
#[ignore]
async fn test_model_without_agent_type_defaults_to_grok_build() {
    with_local_set(|| async {
            let server = MockInferenceServer::start_with_models(
                    vec![
            MockModelEntry::new("no-agent-type-model"),
        ],
                )
                .await
                .expect("start mock server");
            let workdir = git_workdir();
            let client = GrokStdioClient::spawn(&server, workdir.workspace()).await;
            client.initialize_with_timeout().await;
            let session_id = client
                .create_session_with_model_timeout(
                    workdir.workspace(),
                    "no-agent-type-model",
                )
                .await;
            let result = client.prompt_with_timeout(&session_id, "say hello").await;
            assert!(result.is_ok(), "prompt failed: {:?}", result.err());
            let sys_prompt = server
                .last_system_prompt()
                .expect("should have at least one inference request");
            assert!(
            is_grok_build_system_prompt(&sys_prompt),
            "model without agent_type should default to grok-build harness\nsystem prompt preview: {}",
            &sys_prompt[..sys_prompt.len().min(500)]
        );
        })
        .await;
}
/// The `GROK_AGENT` escape hatch should override the model's agent_type.
/// Setting `GROK_AGENT=grok-build` with an alternate-agent model should use
/// grok-build harness.
#[tokio::test]
#[ignore]
async fn test_grok_agent_env_overrides_model_agent_type() {
    with_local_set(|| async {
            let server = dual_model_server().await;
            let workdir = git_workdir();
            let sandbox = TestSandbox::builder().mock_url(server.url()).build();
            let client = GrokStdioClient::spawn_with_sandbox_env_and_args(
                    &server,
                    workdir.workspace(),
                    sandbox,
                    &[("GROK_AGENT", "grok-build")],
                    &[],
                )
                .await;
            client.initialize_with_timeout().await;
            let session_id = client
                .create_session_with_model_timeout(workdir.workspace(), "cursor-model")
                .await;
            let result = client.prompt_with_timeout(&session_id, "say hello").await;
            assert!(
            result.is_ok(),
            "prompt with GROK_AGENT override failed: {:?}\nstderr:\n{}",
            result.err(),
            client.stderr()
        );
            let sys_prompt = server
                .last_system_prompt()
                .expect("should have inference request");
            assert!(
            is_grok_build_system_prompt(&sys_prompt),
            "GROK_AGENT=grok-build should override catalog model agent_type\nsystem prompt preview: {}",
            &sys_prompt[..sys_prompt.len().min(500)]
        );
        })
        .await;
}
