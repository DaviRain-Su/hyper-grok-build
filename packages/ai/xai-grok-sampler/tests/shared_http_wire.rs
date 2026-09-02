mod support;

use std::sync::atomic::Ordering;

use support::{pin_env, send_one, test_config};
use xai_grok_sampler::SamplingClient;
use xai_grok_test_support::spawn_counting_server;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_client_keeps_per_config_headers_isolated() {
    pin_env();
    let (base_url, _accepts, heads) = spawn_counting_server().await;
    let mut cfg_a = test_config(&base_url, "token-a");
    cfg_a
        .extra_headers
        .insert("x-test-extra".to_string(), "isolated-a".to_string());
    let mut cfg_b = test_config(&base_url, "token-b");
    cfg_b
        .extra_headers
        .insert("x-test-extra".to_string(), "isolated-b".to_string());
    let a = SamplingClient::new(cfg_a).unwrap();
    let b = SamplingClient::new(cfg_b).unwrap();
    send_one(&a).await;
    send_one(&b).await;

    let heads = heads.lock().unwrap();
    assert_eq!(heads.len(), 2);
    assert!(heads[0].contains("Bearer token-a") && heads[0].contains("isolated-a"));
    assert!(!heads[0].contains("token-b") && !heads[0].contains("isolated-b"));
    assert!(heads[1].contains("Bearer token-b") && heads[1].contains("isolated-b"));
    assert!(!heads[1].contains("token-a") && !heads[1].contains("isolated-a"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_http1_fallback_never_pools() {
    pin_env();
    let (base_url, accepts, _heads) = spawn_counting_server().await;
    let mut cfg = test_config(&base_url, "token-a");
    cfg.force_http1 = true;
    let client = SamplingClient::new(cfg).unwrap();
    send_one(&client).await;
    send_one(&client).await;
    assert_eq!(accepts.load(Ordering::SeqCst), 2);
}
