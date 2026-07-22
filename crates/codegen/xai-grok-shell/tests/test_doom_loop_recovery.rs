//! Mock-HTTP integration suite for the server-side doom-loop check on the
//! Responses API wire: trigger parsing/dedup onto
//! `ConversationResponse.doom_loop_signals`, the recovery/resample
//! contract, and a headless config-to-header lane.
//!
//! Scripts use `MockInferenceServer`'s FIFO `enqueue_response`: request N
//! consumes script N, so "turn 1 doomed, request 2 is the resample" needs no
//! content-keyed dispatch. Parse tests use non-confident triggers (channel
//! `response`, `low_logprob`, or over-threshold) so they stay orthogonal to
//! the recovery, which acts only on confident signals.

mod common;

use common::{create_test_client, test_sampler_config};
use xai_grok_sampler::RetryPolicy;
use xai_grok_sampling_types::doom_loop::{DoomLoopSignalKind, SAMPLE_CHECK_EVENT_DATA_CUMULATIVE};
use xai_grok_shell::sampling::{
    ApiBackend, Client, ConversationItem, ConversationRequest, RequestId, SamplerActor,
    SamplerHandle,
};
use xai_grok_test_support::sse::{
    responses_api_doom_loop_check_events, responses_api_doom_loop_terminal_only_events,
    responses_api_reasoning_and_text_events, responses_api_reasoning_only_events,
    responses_api_with_doom_loop_frame,
};
use xai_grok_test_support::{MockInferenceServer, MockModelEntry, ScriptedResponse};

const MODEL: &str = "test-model";

/// A sampling client with the doom-loop check enabled (default tunables:
/// `max_threshold` 8, `max_retries` 2).
fn doom_loop_client(base_url: &str) -> Client {
    let mut config = test_sampler_config(base_url, ApiBackend::Responses, &[]);
    config.doom_loop_recovery = Some(Default::default());
    Client::new(config).unwrap()
}

/// A sampler actor (the rung that owns retry/recovery) with the given
/// doom-loop policy. Events are fire-and-forget, so the receiver is dropped.
fn spawn_actor(base_url: &str, doom_loop_enabled: bool) -> SamplerHandle {
    let mut config = test_sampler_config(base_url, ApiBackend::Responses, &[]);
    if doom_loop_enabled {
        config.doom_loop_recovery = Some(Default::default());
    }
    // Small transport budget so a broken spec fails fast instead of spinning.
    let retry = RetryPolicy {
        max_retries: 2,
        rate_limit_retry_threshold: 2,
        ..RetryPolicy::default()
    };
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    SamplerActor::spawn(config, retry, event_tx)
}

fn user_request(text: &str) -> ConversationRequest {
    ConversationRequest::from_items(vec![ConversationItem::user(text)])
}

fn responses_request_count(server: &MockInferenceServer) -> usize {
    server
        .requests()
        .iter()
        .filter(|e| e.method == "POST" && e.path.contains("/responses"))
        .count()
}

// ---------------------------------------------------------------------------
// Trigger parsing (live)
// ---------------------------------------------------------------------------

/// Mid-stream check frames populate `doom_loop_signals`, deduplicated across
/// the cumulative re-sends, with the label grammar fully parsed.
#[tokio::test]
async fn mid_stream_check_frames_populate_and_dedupe_signals() {
    let server = MockInferenceServer::start().await.unwrap();
    server.enqueue_response(
        "/v1/responses",
        ScriptedResponse::sse(responses_api_doom_loop_check_events(
            &["tail_repetition:4@response", "tail_repetition:2@response"],
            "around and around we go",
            MODEL,
        )),
    );
    let client = doom_loop_client(&server.url());

    let response = client
        .conversation_collect(user_request("hello"))
        .await
        .expect("a doomed stream still completes");

    let signals = &response.doom_loop_signals;
    assert_eq!(signals.len(), 2, "cumulative re-sends dedupe by raw label");
    assert_eq!(signals[0].kind, DoomLoopSignalKind::TailRepetition(4));
    assert_eq!(signals[0].channel, "response");
    assert_eq!(signals[0].raw, "tail_repetition:4@response");
    assert_eq!(signals[1].kind, DoomLoopSignalKind::TailRepetition(2));
    // The doomed turn shape itself is preserved: reasoning-only.
    assert!(response.assistant_text().is_empty());
}

/// The inference API's byte-exact cumulative frame parses into both signals
/// through the full HTTP/SSE client path.
#[tokio::test]
async fn byte_exact_cumulative_frame_parses_through_the_client() {
    let server = MockInferenceServer::start().await.unwrap();
    server.enqueue_response(
        "/v1/responses",
        ScriptedResponse::sse(responses_api_with_doom_loop_frame(
            SAMPLE_CHECK_EVENT_DATA_CUMULATIVE,
            "thinking",
            "the answer",
            MODEL,
        )),
    );
    let client = doom_loop_client(&server.url());

    let response = client
        .conversation_collect(user_request("hello"))
        .await
        .unwrap();

    let raws: Vec<&str> = response
        .doom_loop_signals
        .iter()
        .map(|s| s.raw.as_str())
        .collect();
    assert_eq!(
        raws,
        vec!["tail_repetition:4@response", "tail_repetition:2@response"]
    );
    assert!(response.assistant_text().contains("the answer"));
}

/// The terminal-only copy of the signal (no mid-stream frame) also lands on
/// the response.
#[tokio::test]
async fn terminal_only_field_populates_signals() {
    let server = MockInferenceServer::start().await.unwrap();
    server.enqueue_response(
        "/v1/responses",
        ScriptedResponse::sse(responses_api_doom_loop_terminal_only_events(
            &["low_logprob@thinking"],
            "brief thought",
            "an ordinary answer",
            MODEL,
        )),
    );
    let client = doom_loop_client(&server.url());

    let response = client
        .conversation_collect(user_request("hello"))
        .await
        .unwrap();

    assert_eq!(response.doom_loop_signals.len(), 1);
    assert_eq!(
        response.doom_loop_signals[0].kind,
        DoomLoopSignalKind::LowLogprob
    );
    assert_eq!(response.doom_loop_signals[0].channel, "thinking");
    assert!(response.empty_reason().is_none(), "a normal answer turn");
}

/// Malformed check frames are swallowed: the stream completes normally, the
/// answer is intact, and no signal is recorded.
#[tokio::test]
async fn malformed_check_frames_complete_cleanly_without_signals() {
    let malformed = [
        // triggers as a string
        r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":"tail_repetition:8@thinking"}}"#,
        // triggers as a number
        r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":8}}"#,
        // triggers as an array of objects
        r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":[{"kind":"tail_repetition"}]}}"#,
        // missing doom_loop_check key entirely
        r#"{"type":"response.doom_loop_check","sequence_number":9}"#,
        // not JSON at all (only the SSE event name identifies it)
        "definitely not json",
    ];

    let server = MockInferenceServer::start().await.unwrap();
    for payload in malformed {
        let events = responses_api_with_doom_loop_frame(payload, "hm", "fine", MODEL);
        server.enqueue_response("/v1/responses", ScriptedResponse::sse(events));
    }
    let client = doom_loop_client(&server.url());

    for payload in malformed {
        let response = client
            .conversation_collect(user_request("hello"))
            .await
            .unwrap_or_else(|e| panic!("stream must survive malformed frame {payload}: {e}"));
        assert!(
            response.doom_loop_signals.is_empty(),
            "no signal from malformed frame {payload}"
        );
        assert!(response.assistant_text().contains("fine"));
    }
}

/// Unknown extra keys on a well-formed frame do not impede parsing.
#[tokio::test]
async fn unknown_extra_keys_still_parse() {
    let payload = r#"{"sequence_number":7,"type":"response.doom_loop_check","doom_loop_check":{"triggers":["tail_repetition:4@response"]},"future_field":true}"#;
    let server = MockInferenceServer::start().await.unwrap();
    let events = responses_api_with_doom_loop_frame(payload, "hm", "fine", MODEL);
    server.enqueue_response("/v1/responses", ScriptedResponse::sse(events));

    let response = doom_loop_client(&server.url())
        .conversation_collect(user_request("hello"))
        .await
        .unwrap();
    assert_eq!(response.doom_loop_signals.len(), 1);
    assert_eq!(
        response.doom_loop_signals[0].raw,
        "tail_repetition:4@response"
    );
}

/// No check frame and no terminal field: the signal set stays empty.
#[tokio::test]
async fn absent_field_leaves_signals_empty() {
    let server = MockInferenceServer::start().await.unwrap();
    let events = responses_api_reasoning_and_text_events("thinking", "hello", MODEL);
    server.enqueue_response("/v1/responses", ScriptedResponse::sse(events));

    let response = doom_loop_client(&server.url())
        .conversation_collect(user_request("hello"))
        .await
        .unwrap();
    assert!(response.doom_loop_signals.is_empty());
}

/// Label kinds this client version does not know are preserved verbatim as
/// `Unknown` (never dropped, never an error).
#[tokio::test]
async fn unknown_label_kinds_preserved_as_unknown() {
    let server = MockInferenceServer::start().await.unwrap();
    server.enqueue_response(
        "/v1/responses",
        ScriptedResponse::sse(responses_api_doom_loop_check_events(
            &["novel_detector:9@thinking"],
            "hmmm",
            MODEL,
        )),
    );

    let response = doom_loop_client(&server.url())
        .conversation_collect(user_request("hello"))
        .await
        .unwrap();
    assert_eq!(response.doom_loop_signals.len(), 1);
    assert_eq!(
        response.doom_loop_signals[0].kind,
        DoomLoopSignalKind::Unknown("novel_detector:9".to_string())
    );
    assert_eq!(
        response.doom_loop_signals[0].raw,
        "novel_detector:9@thinking"
    );
}

/// With the check disabled, the terminal field is never even parsed — the
/// policy gates all signal work, not just the header.
#[tokio::test]
async fn disabled_policy_leaves_terminal_field_unparsed() {
    let server = MockInferenceServer::start().await.unwrap();
    server.enqueue_response(
        "/v1/responses",
        ScriptedResponse::sse(responses_api_doom_loop_terminal_only_events(
            &["tail_repetition:8@thinking"],
            "thinking",
            "an answer",
            MODEL,
        )),
    );
    let client = create_test_client(&server.url(), ApiBackend::Responses);

    let response = client
        .conversation_collect(user_request("hello"))
        .await
        .unwrap();
    assert!(response.doom_loop_signals.is_empty());
    assert!(response.assistant_text().contains("an answer"));
}

// ---------------------------------------------------------------------------
// Recovery contract (the acceptance spec for the resample behavior)
// ---------------------------------------------------------------------------

/// A confident signal (`tail_repetition:8@thinking` at the default
/// `max_threshold` 8) on a completed turn is resampled once: two requests,
/// the clean second script is the accepted response, and the resample
/// request body is identical to the first — the poisoned turn's output never
/// enters the conversation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn confident_signal_resamples_once_and_discards_poisoned_turn() {
    let server = MockInferenceServer::start().await.unwrap();
    server.enqueue_response(
        "/v1/responses",
        ScriptedResponse::sse(responses_api_doom_loop_terminal_only_events(
            &["tail_repetition:8@thinking"],
            "loop loop loop",
            "poisoned answer",
            MODEL,
        )),
    );
    server.enqueue_response(
        "/v1/responses",
        ScriptedResponse::sse(responses_api_reasoning_and_text_events(
            "fresh thought",
            "clean answer",
            MODEL,
        )),
    );
    let handle = spawn_actor(&server.url(), true);

    let (response, _metrics) = handle
        .submit_and_collect(RequestId::from("doom-confident"), user_request("hello"))
        .await
        .expect("recovery accepts the clean resample");

    assert_eq!(responses_request_count(&server), 2);
    assert_eq!(response.assistant_text(), "clean answer");
    assert!(
        response.doom_loop_signals.is_empty(),
        "the accepted response is the clean resample, not the poisoned turn"
    );
    let bodies = server.request_bodies();
    assert_eq!(
        bodies[0]["input"], bodies[1]["input"],
        "the resample re-sends the same prefix; poisoned output never enters it"
    );
}

/// Budget exhaustion: with `max_retries` 2, three consecutively doomed turns
/// consume the budget and the LAST doomed response is accepted as-is — the
/// turn still succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn budget_exhaustion_accepts_last_doomed_response() {
    let server = MockInferenceServer::start().await.unwrap();
    for _ in 0..3 {
        server.enqueue_response(
            "/v1/responses",
            ScriptedResponse::sse(responses_api_doom_loop_terminal_only_events(
                &["tail_repetition:8@thinking"],
                "loop loop loop",
                "still looping answer",
                MODEL,
            )),
        );
    }
    let handle = spawn_actor(&server.url(), true);

    let (response, _metrics) = handle
        .submit_and_collect(RequestId::from("doom-budget"), user_request("hello"))
        .await
        .expect("an exhausted budget accepts the response instead of erroring");

    assert_eq!(
        responses_request_count(&server),
        3,
        "initial attempt + max_retries (2) resamples"
    );
    assert_eq!(response.assistant_text(), "still looping answer");
    assert!(
        !response.doom_loop_signals.is_empty(),
        "the accepted doomed response keeps its signals (warn-only fallback)"
    );
}

/// Non-confident signals never resample: threshold above `max_threshold`,
/// a non-thinking channel, and `low_logprob` are warn-only. The
/// misclassification fence for the recovery's confidence rule.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_confident_signals_do_not_resample() {
    for trigger in [
        "tail_repetition:64@thinking",
        "tail_repetition:2@response",
        "low_logprob@thinking",
    ] {
        let server = MockInferenceServer::start().await.unwrap();
        server.enqueue_response(
            "/v1/responses",
            ScriptedResponse::sse(responses_api_doom_loop_terminal_only_events(
                &[trigger],
                "some thought",
                "kept answer",
                MODEL,
            )),
        );
        let handle = spawn_actor(&server.url(), true);

        let (response, _metrics) = handle
            .submit_and_collect(RequestId::from("doom-lax"), user_request("hello"))
            .await
            .unwrap();

        assert_eq!(
            responses_request_count(&server),
            1,
            "{trigger} is not confident and must not resample"
        );
        assert_eq!(response.assistant_text(), "kept answer");
        assert_eq!(response.doom_loop_signals[0].raw, trigger);
    }
}

/// A disabled policy ignores even a confident signal end-to-end through the
/// actor: one request, field unparsed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_policy_ignores_confident_signal() {
    let server = MockInferenceServer::start().await.unwrap();
    server.enqueue_response(
        "/v1/responses",
        ScriptedResponse::sse(responses_api_doom_loop_terminal_only_events(
            &["tail_repetition:8@thinking"],
            "loop loop loop",
            "accepted anyway",
            MODEL,
        )),
    );
    let handle = spawn_actor(&server.url(), false);

    let (response, _metrics) = handle
        .submit_and_collect(RequestId::from("doom-disabled"), user_request("hello"))
        .await
        .unwrap();

    assert_eq!(responses_request_count(&server), 1);
    assert_eq!(response.assistant_text(), "accepted anyway");
    assert!(response.doom_loop_signals.is_empty());
}

/// A confident signal arriving mid-stream aborts the attempt and resamples.
/// The early abort itself is not externally assertable; the contract is two
/// requests and the clean final response. The poisoned script carries a
/// visible answer so the existing empty-response retry cannot mask the
/// doom-loop path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mid_stream_signal_aborts_and_resamples() {
    let confident_frame = r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":["tail_repetition:8@thinking"]}}"#;
    let server = MockInferenceServer::start().await.unwrap();
    server.enqueue_response(
        "/v1/responses",
        ScriptedResponse::sse(responses_api_with_doom_loop_frame(
            confident_frame,
            "loop loop loop",
            "poisoned answer",
            MODEL,
        )),
    );
    server.enqueue_response(
        "/v1/responses",
        ScriptedResponse::sse(responses_api_reasoning_and_text_events(
            "fresh thought",
            "clean answer",
            MODEL,
        )),
    );
    let handle = spawn_actor(&server.url(), true);

    let (response, _metrics) = handle
        .submit_and_collect(RequestId::from("doom-midstream"), user_request("hello"))
        .await
        .unwrap();

    assert_eq!(responses_request_count(&server), 2);
    assert_eq!(response.assistant_text(), "clean answer");
}

/// The doom-loop budget and the existing empty-response retry class coexist,
/// one debit each: turn 1 is doomed but NON-empty (confident trigger plus a
/// visible answer), so only the doom class can advance past it; turn 2 is
/// reasoning-only without a trigger, so only the empty class fires; turn 3
/// is the clean accept.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doomed_then_reasoning_only_empty_coexist() {
    let server = MockInferenceServer::start().await.unwrap();
    server.enqueue_response(
        "/v1/responses",
        ScriptedResponse::sse(responses_api_doom_loop_terminal_only_events(
            &["tail_repetition:8@thinking"],
            "loop loop loop",
            "poisoned answer",
            MODEL,
        )),
    );
    server.enqueue_response(
        "/v1/responses",
        ScriptedResponse::sse(responses_api_reasoning_only_events(
            "empty but not doomed",
            MODEL,
        )),
    );
    server.enqueue_response(
        "/v1/responses",
        ScriptedResponse::sse(responses_api_reasoning_and_text_events(
            "fresh thought",
            "clean answer",
            MODEL,
        )),
    );
    let handle = spawn_actor(&server.url(), true);

    let (response, _metrics) = handle
        .submit_and_collect(RequestId::from("doom-coexist"), user_request("hello"))
        .await
        .expect("both retry classes stay within their budgets");

    assert_eq!(responses_request_count(&server), 3);
    assert_eq!(response.assistant_text(), "clean answer");
    assert!(response.doom_loop_signals.is_empty());
}

// ---------------------------------------------------------------------------
// Headless lifecycle lane
// ---------------------------------------------------------------------------

/// `[doom_loop_recovery] enabled = true` in `config.toml` reaches the wire
/// through the real binary: the session TURN request (marked by
/// `x-grok-turn-idx`) carries the opt-in header. Aux side-queries the binary
/// also fires at `/v1/responses` (e.g. session-title generation) must NOT
/// carry it — they collect without the actor's retry loop, so an armed
/// abort there could only fail them, never resample. The recovery behavior
/// itself is covered by the mock-HTTP suite above.
///
/// `#[ignore]` (needs a built binary). Run locally (auto-builds the pager):
/// ```bash
/// cargo test -p xai-grok-shell --test test_doom_loop_recovery -- --ignored
/// ```
#[tokio::test]
#[ignore] // requires pre-built binary; run with --ignored
async fn headless_config_enables_doom_loop_check_header() {
    let models = vec![MockModelEntry::new(MODEL).with_api_backend("responses")];
    let server = MockInferenceServer::start_with_models(models)
        .await
        .expect("start mock server");
    let workdir = xai_grok_test_support::git_workdir();
    let home = tempfile::TempDir::new().unwrap();

    let grok_home = home.path().join(".grok");
    std::fs::create_dir_all(&grok_home).expect("create .grok home");
    std::fs::write(
        grok_home.join("config.toml"),
        "[doom_loop_recovery]\nenabled = true\n",
    )
    .expect("write config.toml");

    let mut cmd = tokio::process::Command::new(xai_grok_test_support::grok_binary());
    cmd.args(["-p", "say hi", "--yolo", "--output-format", "json"])
        .arg("--cwd")
        .arg(workdir.path())
        .current_dir(workdir.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    xai_grok_test_support::env::test_env_cmd_tokio(&mut cmd, &server.url(), home.path());
    cmd.env("GROK_HOME", grok_home);
    // Don't attach to a developer's ambient leader; spawn fresh against the mock.
    cmd.env_remove("GROK_LEADER_SOCKET");

    let result = xai_grok_test_support::run_headless_with_cmd(cmd).await;
    xai_grok_test_support::assert_headless_success(&result, "doom-loop header e2e", Some(&server));

    let requests = server.requests();
    let responses_posts: Vec<_> = requests
        .iter()
        .filter(|e| e.method == "POST" && e.path.contains("/responses"))
        .collect();
    // The session turn carries `x-grok-turn-idx`; aux side-queries (session
    // title, etc.) do not.
    let (turns, aux): (Vec<_>, Vec<_>) = responses_posts
        .into_iter()
        .partition(|e| e.header("x-grok-turn-idx").is_some());
    assert!(
        !turns.is_empty(),
        "no session turn POST /v1/responses logged; requests:\n{}",
        server.request_log_summary()
    );
    for turn in turns {
        assert_eq!(
            turn.header("x-grok-doom-loop-check"),
            Some("true"),
            "[doom_loop_recovery] enabled must reach the turn request header; requests:\n{}",
            server.request_log_summary()
        );
    }
    for side_query in aux {
        assert_eq!(
            side_query.header("x-grok-doom-loop-check"),
            None::<&str>,
            "the session policy must not leak into aux side-query clients; requests:\n{}",
            server.request_log_summary()
        );
    }
}
