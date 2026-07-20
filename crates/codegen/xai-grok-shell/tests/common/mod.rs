//! Shared helpers for xai-grok-shell integration tests.

use xai_grok_shell::sampling::{ApiBackend, Client, SamplerConfig};

/// Create a sampling client configured for a mock server. Shared by the
/// integration tests so the ~30-field `SamplerConfig` literal lives in one
/// place (`SamplerConfig` has no `Default`).
pub fn create_test_client(base_url: &str, api_backend: ApiBackend) -> Client {
    create_test_client_with_extra_headers(base_url, api_backend, &[])
}

/// Like [`create_test_client`] but seeds `SamplerConfig::extra_headers`, so a
/// test can assert that session-injected headers reach the wire.
pub fn create_test_client_with_extra_headers(
    base_url: &str,
    api_backend: ApiBackend,
    extra_headers: &[(&str, &str)],
) -> Client {
    Client::new(test_sampler_config(base_url, api_backend, extra_headers)).unwrap()
}

/// The shared mock-server `SamplerConfig`; tests needing a non-default field
/// (e.g. `doom_loop_recovery`) mutate the returned value before building the
/// client themselves.
pub fn test_sampler_config(
    base_url: &str,
    api_backend: ApiBackend,
    extra_headers: &[(&str, &str)],
) -> SamplerConfig {
    // Shell `Client` is `xai_grok_sampler::SamplingClient`, which takes a
    // `SamplerConfig` directly. Construct one inline here.
    SamplerConfig {
        api_key: Some("test-api-key".to_string()),
        base_url: base_url.to_string(),
        model: "test-model".to_string(),
        max_completion_tokens: Some(1000),
        temperature: Some(0.7),
        top_p: None,
        api_backend,
        auth_scheme: Default::default(),
        extra_headers: extra_headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
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
    }
}
