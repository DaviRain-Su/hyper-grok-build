use super::*;

fn test_manager() -> ModelsManager {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let tmp = std::env::temp_dir().join("grok-test-models-manager");
    let auth_manager = Arc::new(AuthManager::new_test_isolated(
        &tmp,
        GrokComConfig::default(),
    ));
    ModelsManagerBuilder::new(
        None,
        IndexMap::new(),
        acp::ModelId::new("default"),
        auth_manager,
        config::Config::default(),
    )
    .cache(test_cache_manager(&tmp))
    .build()
}

/// Cold manager (no prefetch, isolated cache and auth) over `endpoint`.
fn cold_manager(cfg: config::Config, endpoint: Arc<dyn ModelsEndpoint>) -> ModelsManager {
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new_test_isolated(
        tmp.path(),
        GrokComConfig::default(),
    ));
    ModelsManagerBuilder::new(
        None,
        IndexMap::new(),
        acp::ModelId::new("default"),
        auth_manager,
        cfg,
    )
    .endpoint(endpoint)
    .cache(test_cache_manager(tmp.path()))
    .build()
}

/// Never resolves.
struct HangingEndpoint;
impl ModelsEndpoint for HangingEndpoint {
    fn fetch_models(
        &self,
        _endpoints: config::EndpointsConfig,
        _auth: Option<GrokAuth>,
        _fetch_auth: ModelFetchAuth,
    ) -> ModelsFetchFuture {
        Box::pin(std::future::pending())
    }
}

/// Fails every fetch immediately.
struct FailingEndpoint;
impl ModelsEndpoint for FailingEndpoint {
    fn fetch_models(
        &self,
        _endpoints: config::EndpointsConfig,
        _auth: Option<GrokAuth>,
        _fetch_auth: ModelFetchAuth,
    ) -> ModelsFetchFuture {
        Box::pin(async { None })
    }
}

/// Serves `catalog` after `delay`.
struct SlowEndpoint {
    catalog: IndexMap<String, ModelEntry>,
    delay: std::time::Duration,
}
impl ModelsEndpoint for SlowEndpoint {
    fn fetch_models(
        &self,
        _endpoints: config::EndpointsConfig,
        _auth: Option<GrokAuth>,
        _fetch_auth: ModelFetchAuth,
    ) -> ModelsFetchFuture {
        let catalog = self.catalog.clone();
        let delay = self.delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            Some(catalog)
        })
    }
}

#[tokio::test]
async fn catalog_retry_recovers_after_endpoint_returns() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RecoveringEndpoint {
        calls: Arc<AtomicUsize>,
        catalog: IndexMap<String, ModelEntry>,
    }
    impl ModelsEndpoint for RecoveringEndpoint {
        fn fetch_models(
            &self,
            _endpoints: config::EndpointsConfig,
            _auth: Option<GrokAuth>,
            _fetch_auth: ModelFetchAuth,
        ) -> ModelsFetchFuture {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let out = if n == 0 {
                None
            } else {
                Some(self.catalog.clone())
            };
            Box::pin(async move { out })
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let tmp = std::env::temp_dir().join("grok-test-catalog-retry");
    let auth_manager = Arc::new(AuthManager::new_test_isolated(
        &tmp,
        GrokComConfig::default(),
    ));
    let mgr = ModelsManagerBuilder::new(
        None,
        IndexMap::new(),
        acp::ModelId::new("default"),
        auth_manager,
        config::Config::default(),
    )
    .endpoint(Arc::new(RecoveringEndpoint {
        calls: calls.clone(),
        catalog: make_prefetched(&["grok-4"]),
    }))
    .build();
    assert!(!mgr.has_fetched_real_catalog());

    mgr.spawn_catalog_retry_with_backoff(
        /*remote_fetch_enabled*/ true,
        crate::tools::retry::BackoffConfig::new(5, 1, 10),
    );

    let mut recovered = false;
    for _ in 0..200 {
        if mgr.has_fetched_real_catalog() {
            recovered = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        recovered,
        "catalog retry did not recover after the endpoint returned"
    );
    assert!(mgr.models().contains_key("grok-4"));
    assert!(
        calls.load(Ordering::SeqCst) >= 2,
        "expected a failed attempt then a success",
    );
}

#[tokio::test]
async fn disk_cache_reload_applies_without_fetching() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingEndpoint {
        calls: Arc<AtomicUsize>,
    }
    impl ModelsEndpoint for CountingEndpoint {
        fn fetch_models(
            &self,
            _endpoints: config::EndpointsConfig,
            _auth: Option<GrokAuth>,
            _fetch_auth: ModelFetchAuth,
        ) -> ModelsFetchFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { None })
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new_test_isolated(
        tmp.path(),
        GrokComConfig::default(),
    ));
    let mgr = ModelsManagerBuilder::new(
        None,
        IndexMap::new(),
        acp::ModelId::new("default"),
        auth_manager,
        config_from_toml("[models]\ndefault = \"grok-4.5\""),
    )
    .endpoint(Arc::new(CountingEndpoint {
        calls: calls.clone(),
    }))
    .cache(test_cache_manager(tmp.path()))
    .build();

    let seeder = test_cache_manager(tmp.path());
    let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
    seeder.persist(
        &make_prefetched(&["grok-4.5"]),
        Some("etag-x"),
        auth_method,
        &mgr.cache_origin(),
    );

    mgr.reload_from_disk_cache();

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the disk cache load must never hit the transport",
    );
    assert!(mgr.models().contains_key("grok-4.5"));
    assert!(mgr.has_fetched_real_catalog());
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4.5",
        "first real catalog from the disk cache must resolve the configured default",
    );
}

#[tokio::test]
async fn auth_refresh_watcher_refetches_on_notify() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct NotifyEndpoint {
        calls: Arc<AtomicUsize>,
        catalog: IndexMap<String, ModelEntry>,
    }
    impl ModelsEndpoint for NotifyEndpoint {
        fn fetch_models(
            &self,
            _endpoints: config::EndpointsConfig,
            _auth: Option<GrokAuth>,
            _fetch_auth: ModelFetchAuth,
        ) -> ModelsFetchFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let catalog = self.catalog.clone();
            Box::pin(async move { Some(catalog) })
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let tmp = std::env::temp_dir().join("grok-test-auth-refresh-watcher");
    let auth_manager = Arc::new(AuthManager::new_test_isolated(
        &tmp,
        GrokComConfig::default(),
    ));
    let mgr = ModelsManagerBuilder::new(
        None,
        IndexMap::new(),
        acp::ModelId::new("default"),
        auth_manager,
        config::Config::default(),
    )
    .endpoint(Arc::new(NotifyEndpoint {
        calls: calls.clone(),
        catalog: make_prefetched(&["grok-4"]),
    }))
    .build();
    assert!(!mgr.has_fetched_real_catalog());

    let notify = Arc::new(tokio::sync::Notify::new());
    mgr.start_auth_refresh_watcher(notify.clone());
    notify.notify_one();

    let mut updated = false;
    for _ in 0..200 {
        if mgr.has_fetched_real_catalog() {
            updated = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(updated, "watcher did not re-fetch the catalog on notify");
    assert!(mgr.models().contains_key("grok-4"));
    assert!(calls.load(Ordering::SeqCst) >= 1);
}

#[tokio::test(start_paused = true)]
async fn hanging_fetch_does_not_block_refresh() {
    let mgr = cold_manager(config::Config::default(), Arc::new(HangingEndpoint));

    tokio::time::timeout(
        crate::http::STARTUP_FETCH_TIMEOUT * 10,
        mgr.fetch_and_apply_inner(/*remote_fetch_enabled*/ true),
    )
    .await
    .expect("fetch_and_apply_inner must return despite a hanging endpoint");

    assert!(
        !mgr.has_fetched_real_catalog(),
        "a timed-out fetch must not mark a real catalog",
    );
}

#[tokio::test(start_paused = true)]
async fn slow_fetch_within_timeout_still_applies() {
    // "Slow but succeeds": a fetch that returns just under STARTUP_FETCH_TIMEOUT
    // must still be applied, not degraded to offline.
    let mgr = cold_manager(
        config::Config::default(),
        Arc::new(SlowEndpoint {
            catalog: make_prefetched(&["grok-4"]),
            delay: crate::http::STARTUP_FETCH_TIMEOUT / 2,
        }),
    );

    mgr.fetch_and_apply_inner(/*remote_fetch_enabled*/ true)
        .await;
    assert!(
        mgr.has_fetched_real_catalog(),
        "a fetch within the timeout must apply, not degrade",
    );
    assert!(mgr.models().contains_key("grok-4"));
}

#[tokio::test(start_paused = true)]
async fn etag_refresh_is_bounded_and_single_flighted() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingHangEndpoint {
        calls: Arc<AtomicUsize>,
    }
    impl ModelsEndpoint for CountingHangEndpoint {
        fn fetch_models(
            &self,
            _endpoints: config::EndpointsConfig,
            _auth: Option<GrokAuth>,
            _fetch_auth: ModelFetchAuth,
        ) -> ModelsFetchFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::pending())
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new_test_isolated(
        tmp.path(),
        GrokComConfig::default(),
    ));
    let mgr = ModelsManagerBuilder::new(
        None,
        IndexMap::new(),
        acp::ModelId::new("default"),
        auth_manager,
        config::Config::default(),
    )
    .endpoint(Arc::new(CountingHangEndpoint {
        calls: calls.clone(),
    }))
    .build();

    // First etag change spawns a bounded fetch; let the task register in-flight.
    mgr.spawn_fetch_inner(Some("etag-1".into()), /*remote_fetch_enabled*/ true);
    tokio::task::yield_now().await;
    // Single-flight: a second spawn while one is in flight must not fetch again.
    mgr.spawn_fetch_inner(Some("etag-2".into()), /*remote_fetch_enabled*/ true);
    tokio::task::yield_now().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "single-flight: only one etag fetch in flight at a time",
    );

    // Advance past the bound so the hung fetch is abandoned and the guard clears.
    tokio::time::sleep(crate::http::STARTUP_FETCH_TIMEOUT * 2).await;
    tokio::task::yield_now().await;

    // Guard released → a later etag change fetches again.
    mgr.spawn_fetch_inner(Some("etag-3".into()), /*remote_fetch_enabled*/ true);
    tokio::task::yield_now().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "after the timeout cleared the in-flight guard, a new etag fetch proceeds",
    );

    // remote_fetch disabled is a no-op: no additional fetch.
    mgr.spawn_fetch_inner(Some("etag-4".into()), /*remote_fetch_enabled*/ false);
    tokio::task::yield_now().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "disabled gate must not fetch"
    );
}

#[tokio::test(start_paused = true)]
async fn first_catalog_wait_unblocks_on_fetch_and_skips_dead_dwell() {
    // Deployment auth: a fetch can succeed without a session, so the wait
    // dwells regardless of ambient API-key env.
    let mgr = cold_manager(
        config_from_toml("[endpoints]\ndeployment_key = \"deploy-key\""),
        Arc::new(SlowEndpoint {
            catalog: make_prefetched(&["grok-4"]),
            delay: crate::http::STARTUP_FETCH_TIMEOUT / 2,
        }),
    );

    // Cold cache, remote fetch disabled: no fetch is coming, so no dwell.
    let start = tokio::time::Instant::now();
    assert!(
        !mgr.wait_for_first_catalog_inner(/*remote_fetch_enabled*/ false)
            .await
    );
    assert_eq!(start.elapsed(), std::time::Duration::ZERO);

    // Cold cache, no attempt spawned: nothing to wait for, so no dwell.
    let start = tokio::time::Instant::now();
    assert!(
        !mgr.wait_for_first_catalog_inner(/*remote_fetch_enabled*/ true)
            .await
    );
    assert_eq!(start.elapsed(), std::time::Duration::ZERO);

    // Cold cache, fetch in flight: the wait unblocks when the fetch lands.
    mgr.spawn_fetch_inner(None, /*remote_fetch_enabled*/ true);
    assert!(
        mgr.wait_for_first_catalog_inner(/*remote_fetch_enabled*/ true)
            .await,
        "the wait must observe the completed fetch",
    );
    assert!(mgr.models().contains_key("grok-4"));

    // Warm: an already-loaded catalog returns immediately.
    let start = tokio::time::Instant::now();
    assert!(
        mgr.wait_for_first_catalog_inner(/*remote_fetch_enabled*/ true)
            .await
    );
    assert_eq!(start.elapsed(), std::time::Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn first_catalog_wait_unblocks_on_failed_fetch() {
    let mgr = cold_manager(
        config_from_toml("[endpoints]\ndeployment_key = \"deploy-key\""),
        Arc::new(FailingEndpoint),
    );
    let budget = crate::http::STARTUP_AUTH_REFRESH_TIMEOUT + crate::http::STARTUP_FETCH_TIMEOUT;
    let start = tokio::time::Instant::now();
    mgr.spawn_fetch_inner(None, /*remote_fetch_enabled*/ true);
    assert!(
        !mgr.wait_for_first_catalog_inner(/*remote_fetch_enabled*/ true)
            .await
    );
    assert!(start.elapsed() < budget, "failure must beat the budget");
}

#[tokio::test(start_paused = true)]
async fn first_catalog_wait_is_bounded() {
    let mgr = cold_manager(
        config_from_toml("[endpoints]\ndeployment_key = \"deploy-key\""),
        Arc::new(HangingEndpoint),
    );
    let budget = crate::http::STARTUP_AUTH_REFRESH_TIMEOUT + crate::http::STARTUP_FETCH_TIMEOUT;
    let _attempt = FetchAttemptGuard::begin(&mgr.inner);
    let start = tokio::time::Instant::now();
    assert!(
        !mgr.wait_for_first_catalog_inner(/*remote_fetch_enabled*/ true)
            .await
    );
    assert_eq!(start.elapsed(), budget, "only the budget ends this wait");
}

#[tokio::test(start_paused = true)]
#[serial]
async fn first_catalog_wait_skips_doomed_signed_out_fetch() {
    let _no_key = EnvGuard::unset("XAI_API_KEY");
    let _no_legacy_key = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
    let mgr = cold_manager(config::Config::default(), Arc::new(HangingEndpoint));
    let start = tokio::time::Instant::now();
    mgr.spawn_fetch_inner(None, /*remote_fetch_enabled*/ true);
    assert!(
        !mgr.wait_for_first_catalog_inner(/*remote_fetch_enabled*/ true)
            .await
    );
    assert_eq!(start.elapsed(), std::time::Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn first_catalog_wait_observes_inline_fetch() {
    let mgr = cold_manager(
        config_from_toml("[endpoints]\ndeployment_key = \"deploy-key\""),
        Arc::new(SlowEndpoint {
            catalog: make_prefetched(&["grok-4"]),
            delay: crate::http::STARTUP_FETCH_TIMEOUT / 2,
        }),
    );
    // Fetch first in the join, so its attempt registers on first poll.
    let ((), ready) = tokio::join!(
        mgr.fetch_and_apply_inner(/*remote_fetch_enabled*/ true),
        mgr.wait_for_first_catalog_inner(/*remote_fetch_enabled*/ true),
    );
    assert!(ready, "the wait must observe the inline fetch's outcome");
}

#[tokio::test(start_paused = true)]
async fn new_fetch_attempt_supersedes_failed_latch() {
    let mgr = cold_manager(
        config_from_toml("[endpoints]\ndeployment_key = \"deploy-key\""),
        Arc::new(FailingEndpoint),
    );
    mgr.fetch_and_apply_inner(/*remote_fetch_enabled*/ true)
        .await;
    assert_eq!(
        *mgr.inner.catalog_progress.borrow(),
        CatalogProgress::Failed
    );

    let attempt = FetchAttemptGuard::begin(&mgr.inner);
    assert_eq!(
        *mgr.inner.catalog_progress.borrow(),
        CatalogProgress::Pending,
        "a new attempt must supersede the stale failure",
    );
    drop(attempt);
    assert_eq!(
        *mgr.inner.catalog_progress.borrow(),
        CatalogProgress::Failed,
        "the last attempt out without an outcome must latch",
    );

    let start = tokio::time::Instant::now();
    assert!(
        !mgr.wait_for_first_catalog_inner(/*remote_fetch_enabled*/ true)
            .await
    );
    assert_eq!(start.elapsed(), std::time::Duration::ZERO);
}

#[test]
fn stale_fetch_result_is_discarded_after_identity_change() {
    let mgr = test_manager();
    let cfg = config::Config::default();
    let stale_generation = mgr.inner.catalog.read().generation;
    mgr.clear();

    assert!(!mgr.apply_refresh_result_fenced(
        &cfg,
        Some(make_prefetched(&["stale-model"])),
        None,
        stale_generation,
    ));
    assert!(!mgr.models().contains_key("stale-model"));
    assert!(!mgr.has_fetched_real_catalog());

    assert!(!mgr.apply_refresh_result_fenced(&cfg, None, None, stale_generation));
    assert_eq!(
        *mgr.inner.catalog_progress.borrow(),
        CatalogProgress::Pending,
        "a stale failure must not latch",
    );

    assert!(mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["new-model"])), None));
    assert!(mgr.models().contains_key("new-model"));
}

fn config_from_toml(toml: &str) -> config::Config {
    config::Config::new_from_toml_cfg(&toml::from_str(toml).unwrap()).unwrap()
}

#[test]
fn available_marks_config_owned_models_for_client_side_reload() {
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new_test_isolated(
        tmp.path(),
        GrokComConfig::default(),
    ));
    let cfg = config_from_toml(
        r#"
            [model.centos]
            model = "gpt-5-codex"
            name = "CentOS Codex"
            base_url = "https://example.invalid/v1"
            api_key = "test-only"
            context_window = 200000
        "#,
    );
    let manager = ModelsManager::from_config(&cfg, None, auth_manager).unwrap();

    let info = manager
        .available()
        .get(&acp::ModelId::new("centos"))
        .cloned()
        .expect("config model should be visible");
    assert_eq!(
        info.meta
            .as_ref()
            .and_then(|meta| meta.get(config::CONFIG_MODEL_META_KEY))
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
}

/// Hermetic platform-credential isolation for tests that assert no-credentials
/// behavior: unset every registry platform's API-key env vars and redirect
/// `GROK_AUTH_PATH` to a scratch dir.
fn isolate_platform_credentials() -> (tempfile::TempDir, Vec<xai_grok_test_support::EnvGuard>) {
    let dir = tempfile::tempdir().unwrap();
    let mut guards = vec![xai_grok_test_support::EnvGuard::set(
        "GROK_AUTH_PATH",
        dir.path().join("auth.json").to_str().unwrap(),
    )];
    guards.extend(xai_grok_test_support::unset_all_byok_platform_api_key_envs());
    (dir, guards)
}

#[test]
fn model_show_model_fingerprint_reads_catalog_flag() {
    let mgr = test_manager();

    let mut flagged = ModelEntry {
        info: config::ModelInfo::fallback("fp-model"),
        api_key: None,
        env_key: None,
        auth_provider: None,
        platform_oauth_active: false,
        api_base_url: None,
    };
    flagged.info.show_model_fingerprint = true;
    mgr.insert_test_entry("fp-model", flagged);

    mgr.insert_test_entry(
        "plain-model",
        ModelEntry {
            info: config::ModelInfo::fallback("plain-model"),
            api_key: None,
            env_key: None,
            auth_provider: None,
            platform_oauth_active: false,
            api_base_url: None,
        },
    );

    let mut custom = ModelEntry {
        info: config::ModelInfo::fallback("enterprise-slug"),
        api_key: None,
        env_key: None,
        auth_provider: None,
        platform_oauth_active: false,
        api_base_url: None,
    };
    custom.info.show_model_fingerprint = true;
    mgr.insert_test_entry("enterprise-key", custom);

    assert!(mgr.model_show_model_fingerprint("fp-model"));
    assert!(!mgr.model_show_model_fingerprint("plain-model"));
    assert!(!mgr.model_show_model_fingerprint("missing-model"));
    assert!(
        mgr.model_show_model_fingerprint("enterprise-slug"),
        "slug lookup must resolve to the catalog key and read the flag",
    );
    assert!(mgr.model_show_model_fingerprint("enterprise-key"));
}

#[test]
fn reasoning_effort_helpers_resolve_wire_name_to_catalog_key() {
    let mgr = test_manager();

    let mut custom = ModelEntry {
        info: config::ModelInfo::fallback("enterprise-slug"),
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
    };
    custom.info.supports_reasoning_effort = true;
    custom.info.reasoning_effort = Some(ReasoningEffort::High);
    custom.info.reasoning_efforts = vec![ReasoningEffortOption {
        id: "high".into(),
        value: ReasoningEffort::High,
        label: "High".into(),
        description: None,
        default: true,
    }];
    mgr.insert_test_entry("enterprise-key", custom);

    for id in ["enterprise-key", "enterprise-slug"] {
        assert!(mgr.model_supports_reasoning_effort(id));
        assert_eq!(
            mgr.model_default_reasoning_effort(id),
            Some(ReasoningEffort::High)
        );
        assert_eq!(mgr.model_reasoning_efforts(id).len(), 1);
    }

    assert!(!mgr.model_supports_reasoning_effort("missing-model"));
    assert_eq!(mgr.model_default_reasoning_effort("missing-model"), None);
    assert!(mgr.model_reasoning_efforts("missing-model").is_empty());
}

#[test]
fn default_model_honors_allowlist_when_no_default_set() {
    let cfg = config_from_toml(
        r#"
            [models]
            allowed_models = ["keep-*"]
            [model.zzz-first]
            model = "zzz-first"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            [model.keep-one]
            model = "keep-one"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            "#,
    );
    let catalog = resolve_model_catalog(&cfg, None);
    let (_key, entry, _src) = resolve_default_model(&cfg, &catalog, true);
    assert!(
        entry.info.user_selectable,
        "picked non-selectable {}",
        entry.model
    );
}

#[test]
fn available_models_projects_locked_platform_models_with_setup_meta() {
    use xai_grok_sampling_types::{
        API_KEY_ENV_META_KEY, PLATFORM_ID_META_KEY, PLATFORM_NAME_META_KEY,
        REQUIRES_API_KEY_META_KEY, SETUP_HINT_META_KEY,
    };
    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    let platform_entry = |slug: &str, key: Option<&str>, hidden: bool| {
        let mut info = config::ModelInfo::fallback(slug);
        info.hidden = hidden;
        ModelEntry {
            info,
            api_key: key.map(str::to_owned),
            env_key: None,
            auth_provider: None,
            platform_oauth_active: false,
            api_base_url: None,
        }
    };
    catalog.insert(
        "deepseek/deepseek-v4-flash".to_string(),
        platform_entry("deepseek/deepseek-v4-flash", None, false),
    );
    catalog.insert(
        "ant-ling/Ling-2.6-flash".to_string(),
        platform_entry("ant-ling/Ling-2.6-flash", None, false),
    );
    catalog.insert(
        "openai/gpt-5".to_string(),
        platform_entry("openai/gpt-5", Some("sk-test"), false),
    );
    catalog.insert(
        "groq/llama-3.3-70b-versatile".to_string(),
        platform_entry("groq/llama-3.3-70b-versatile", None, true),
    );

    let out = available_models(&catalog, false);

    let locked_id = acp::ModelId::new("deepseek/deepseek-v4-flash");
    let locked = out
        .get(&locked_id)
        .expect("locked platform model must be projected");
    let meta = locked.meta.as_ref().expect("lock meta present");
    assert_eq!(
        meta.get(REQUIRES_API_KEY_META_KEY)
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        meta.get(PLATFORM_ID_META_KEY).and_then(|v| v.as_str()),
        Some("deepseek")
    );
    assert_eq!(
        meta.get(API_KEY_ENV_META_KEY)
            .and_then(|v| v.as_array())
            .map(|a| a.len()),
        Some(2)
    );
    assert!(
        meta.get(SETUP_HINT_META_KEY)
            .and_then(|v| v.as_str())
            .is_some_and(|h| h.contains("[platforms.deepseek]"))
    );

    let registry_only = out
        .get(&acp::ModelId::new("ant-ling/Ling-2.6-flash"))
        .expect("registry-only provider model must be projected");
    let registry_only_meta = registry_only.meta.as_ref().expect("lock meta present");
    assert_eq!(
        registry_only_meta
            .get(PLATFORM_ID_META_KEY)
            .and_then(|value| value.as_str()),
        Some("ant-ling")
    );
    assert_eq!(
        registry_only_meta
            .get(PLATFORM_NAME_META_KEY)
            .and_then(|value| value.as_str()),
        Some("Ant Ling")
    );
    assert_eq!(
        registry_only_meta
            .get(API_KEY_ENV_META_KEY)
            .and_then(|value| value.as_array())
            .map(Vec::len),
        Some(2)
    );

    let usable = out
        .get(&acp::ModelId::new("openai/gpt-5"))
        .expect("credentialed entry stays visible");
    let usable_meta = usable.meta.as_ref().expect("meta present");
    assert!(
        !usable_meta
            .get(REQUIRES_API_KEY_META_KEY)
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    );

    assert!(
        out.get(&acp::ModelId::new("groq/llama-3.3-70b-versatile"))
            .is_none()
    );

    let keys: Vec<_> = out.keys().map(|k| k.0.as_ref().to_string()).collect();
    assert_eq!(
        keys,
        [
            "openai/gpt-5",
            "deepseek/deepseek-v4-flash",
            "ant-ling/Ling-2.6-flash",
        ]
    );
}

#[test]
fn validate_selectable_rejects_bad_allowlists() {
    let excluded = config_from_toml(
        r#"
            [models]
            default = "grok-3"
            allowed_models = ["grok-4*"]
            [model.grok-3]
            model = "grok-3"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            [model.grok-4]
            model = "grok-4"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            "#,
    );
    let catalog = resolve_model_catalog(&excluded, None);
    assert!(
        validate_selectable(&excluded, &catalog)
            .unwrap_err()
            .contains("grok-3")
    );

    let zero = config_from_toml(
        r#"
            [models]
            allowed_models = ["nomatch-*"]
            [model.grok-4]
            model = "grok-4"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            "#,
    );
    let catalog = resolve_model_catalog(&zero, None);
    assert!(validate_selectable(&zero, &catalog).is_err());
}

#[tokio::test]
async fn refresh_if_new_etag_skips_when_same() {
    let mgr = test_manager();
    mgr.inner.catalog.write().etag = Some("\"abc123\"".to_string());

    mgr.refresh_if_new_etag("\"abc123\"".to_string()).await;
    assert_eq!(
        mgr.inner.catalog.read().etag.as_deref(),
        Some("\"abc123\""),
        "etag should remain unchanged when same"
    );
}

#[tokio::test]
async fn set_current_model_id_change_fires_watch_to_all_subscribers() {
    let mgr = test_manager();
    let mut rx_a = mgr.subscribe_model_switch();
    let mut rx_b = mgr.subscribe_model_switch();
    let initial_a = *rx_a.borrow_and_update();
    let initial_b = *rx_b.borrow_and_update();
    assert_eq!(initial_a, initial_b);

    mgr.set_current_model_id(acp::ModelId::new("default"));
    let same_id_ticked = tokio::time::timeout(std::time::Duration::from_millis(25), rx_a.changed())
        .await
        .is_ok();
    assert!(
        !same_id_ticked,
        "set_current_model_id(same id) must NOT bump the watch generation",
    );

    mgr.set_current_model_id(acp::ModelId::new("grok-4"));
    tokio::time::timeout(std::time::Duration::from_millis(100), rx_a.changed())
        .await
        .expect("rx_a saw the switch")
        .expect("watch channel still open");
    tokio::time::timeout(std::time::Duration::from_millis(100), rx_b.changed())
        .await
        .expect("rx_b saw the switch")
        .expect("watch channel still open");
    assert_ne!(*rx_a.borrow(), initial_a);
    assert_eq!(*rx_a.borrow(), *rx_b.borrow());
    assert!(mgr.model_switch_generation() > initial_a);
}

#[tokio::test]
async fn model_switch_generation_snapshot_reflects_current_state() {
    let mgr = test_manager();
    let start = mgr.model_switch_generation();
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));
    assert_eq!(mgr.model_switch_generation(), start + 1);
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));
    assert_eq!(mgr.model_switch_generation(), start + 1);
    mgr.set_current_model_id(acp::ModelId::new("grok-3"));
    assert_eq!(mgr.model_switch_generation(), start + 2);
}

#[test]
fn first_catalog_reselect_bumps_model_switch_watch() {
    let mgr = test_manager();
    let start = mgr.model_switch_generation();
    let cfg = config_from_toml("[models]\ndefault = \"grok-4.5\"");
    mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["grok-4.5", "grok-4"])), None);
    assert_eq!(mgr.current_model_id().0.as_ref(), "grok-4.5");
    assert!(
        mgr.model_switch_generation() > start,
        "background reselection must fire the model-switch watch",
    );
}

#[test]
fn reselect_missing_current_model_bumps_watch() {
    let mgr = test_manager();
    let cfg = config::Config::default();
    mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["grok-4", "grok-3"])), None);
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));
    let start = mgr.model_switch_generation();
    // A later catalog drops the current model → reselect_current_model_if_missing.
    mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["grok-3"])), None);
    assert_ne!(mgr.current_model_id().0.as_ref(), "grok-4");
    assert!(
        mgr.model_switch_generation() > start,
        "reselecting away from a removed current model must fire the watch",
    );
}

#[test]
fn reselect_locked_current_platform_model_bumps_watch() {
    let mgr = test_manager();
    let cfg = config::Config::default();
    let mut models = make_prefetched(&["grok-3"]);
    let locked_id = "openai/gpt-5";
    models.insert(locked_id.to_string(), make_model_entry(locked_id));
    mgr.inner.catalog.write().models = models;
    mgr.set_current_model_id(acp::ModelId::new(locked_id));
    let start = mgr.model_switch_generation();

    mgr.reselect_current_model_if_missing(&cfg);

    assert_ne!(mgr.current_model_id().0.as_ref(), locked_id);
    assert_eq!(mgr.current_model_id().0.as_ref(), "grok-3");
    assert!(
        mgr.model_switch_generation() > start,
        "reselecting away from a credential-locked platform model must fire the watch",
    );
}

#[test]
fn rebuild_updates_models_and_available() {
    let mgr = test_manager();
    assert!(mgr.models().is_empty());
    assert!(mgr.available().is_empty());

    let cfg = config::Config::default();
    let mut prefetched = IndexMap::new();
    prefetched.insert(
        "test-model".to_string(),
        ModelEntry {
            info: config::ModelInfo::fallback("test-model"),
            api_key: None,
            env_key: None,
            auth_provider: None,
            platform_oauth_active: false,
            api_base_url: None,
        },
    );

    mgr.rebuild(&cfg, Some(prefetched));

    assert!(
        !mgr.models().is_empty(),
        "models should be populated after rebuild"
    );
}

#[test]
fn current_reasoning_effort_round_trip() {
    let mgr = test_manager();
    assert_eq!(mgr.current_reasoning_effort(), None);

    mgr.set_current_reasoning_effort(Some(ReasoningEffort::High));
    assert_eq!(mgr.current_reasoning_effort(), Some(ReasoningEffort::High));

    mgr.set_current_reasoning_effort(None);
    assert_eq!(mgr.current_reasoning_effort(), None);
}

#[test]
fn current_reasoning_effort_seeded_from_config() {
    let tmp = std::env::temp_dir().join("grok-test-models-manager-seed");
    let auth_manager = Arc::new(AuthManager::new_test_isolated(
        &tmp,
        GrokComConfig::default(),
    ));
    let mut cfg = config::Config::default();
    cfg.models.default_reasoning_effort = Some(ReasoningEffort::Xhigh);
    let mgr = ModelsManager::new(
        None,
        IndexMap::new(),
        acp::ModelId::new("default"),
        auth_manager,
        cfg,
    );
    assert_eq!(mgr.current_reasoning_effort(), Some(ReasoningEffort::Xhigh),);
}

#[test]
fn default_reasoning_effort_only_stamps_supporting_model() {
    use indexmap::IndexMap;

    let mut cfg = config::Config::default();
    cfg.models.default = Some("reasoning-model".to_string());
    cfg.models.default_reasoning_effort = Some(ReasoningEffort::High);

    let mut prefetched = IndexMap::new();
    let mut reasoning_entry = ModelEntry {
        info: config::ModelInfo::fallback("reasoning-model"),
        api_key: None,
        env_key: None,
        auth_provider: None,
        platform_oauth_active: false,
        api_base_url: None,
    };
    reasoning_entry.info.supports_reasoning_effort = true;
    prefetched.insert("reasoning-model".to_string(), reasoning_entry);

    let catalog = resolve_model_catalog(&cfg, Some(prefetched));
    assert_eq!(
        catalog["reasoning-model"].info.reasoning_effort,
        Some(ReasoningEffort::High),
        "reasoning-supporting default model should be stamped",
    );

    let mut cfg = config::Config::default();
    cfg.models.default = Some("plain-model".to_string());
    cfg.models.default_reasoning_effort = Some(ReasoningEffort::High);

    let mut prefetched = IndexMap::new();
    let plain_entry = ModelEntry {
        info: config::ModelInfo::fallback("plain-model"),
        api_key: None,
        env_key: None,
        auth_provider: None,
        platform_oauth_active: false,
        api_base_url: None,
    };
    prefetched.insert("plain-model".to_string(), plain_entry);

    let catalog = resolve_model_catalog(&cfg, Some(prefetched));
    assert_eq!(
        catalog["plain-model"].info.reasoning_effort, None,
        "non-reasoning default model must NOT be stamped with persisted effort",
    );
}

#[test]
fn reasoning_effort_override_skips_models_that_do_not_offer_level() {
    use indexmap::IndexMap;
    use xai_grok_sampling_types::ReasoningEffortOption;

    let cfg = config::Config {
        reasoning_effort_override: Some(ReasoningEffort::None),
        ..Default::default()
    };

    let mut prefetched = IndexMap::new();
    let mut no_none = ModelEntry {
        info: config::ModelInfo::fallback("grok-4.5"),
        api_key: None,
        env_key: None,
        auth_provider: None,
        platform_oauth_active: false,
        api_base_url: None,
    };
    no_none.info.supports_reasoning_effort = true;
    no_none.info.reasoning_efforts = vec![ReasoningEffortOption {
        id: "high".into(),
        value: ReasoningEffort::High,
        label: "High".into(),
        description: None,
        default: true,
    }];
    no_none.info.reasoning_effort = Some(ReasoningEffort::High);
    prefetched.insert("grok-4.5".to_string(), no_none);

    let mut with_none = ModelEntry {
        info: config::ModelInfo::fallback("legacy-none"),
        api_key: None,
        env_key: None,
        auth_provider: None,
        platform_oauth_active: false,
        api_base_url: None,
    };
    with_none.info.supports_reasoning_effort = true;
    with_none.info.reasoning_efforts = vec![ReasoningEffortOption {
        id: "none".into(),
        value: ReasoningEffort::None,
        label: "None".into(),
        description: None,
        default: true,
    }];
    prefetched.insert("legacy-none".to_string(), with_none);

    let catalog = resolve_model_catalog(&cfg, Some(prefetched));
    assert_eq!(
        catalog["grok-4.5"].info.reasoning_effort,
        Some(ReasoningEffort::High),
        "--effort none must not stamp onto models that do not offer none"
    );
    assert_eq!(
        catalog["legacy-none"].info.reasoning_effort,
        Some(ReasoningEffort::None),
        "models that list none should still accept the override"
    );
}

#[test]
fn config_menu_only_model_derives_support_and_default() {
    let mut cfg = config::Config::default();
    cfg.config_models.insert(
        "menu-only".to_string(),
        config::ConfigModelOverride {
            reasoning_efforts: vec![
                ReasoningEffortOption {
                    id: "balanced".to_string(),
                    value: ReasoningEffort::Medium,
                    label: "Balanced".to_string(),
                    description: None,
                    default: false,
                },
                ReasoningEffortOption {
                    id: "deep".to_string(),
                    value: ReasoningEffort::Xhigh,
                    label: "Deep".to_string(),
                    description: None,
                    default: true,
                },
            ],
            ..Default::default()
        },
    );
    cfg.config_models
        .insert("plain".to_string(), config::ConfigModelOverride::default());

    let catalog = resolve_model_catalog(&cfg, None);
    let info = &catalog["menu-only"].info;
    assert!(
        info.supports_reasoning_effort,
        "menu-only model must derive support"
    );
    assert_eq!(
        info.reasoning_effort,
        Some(ReasoningEffort::Xhigh),
        "derived default = marked-default option value"
    );
    assert!(!catalog["plain"].info.supports_reasoning_effort);
    assert_eq!(catalog["plain"].info.reasoning_effort, None);

    let tmp = std::env::temp_dir().join("grok-test-models-manager-menu-only");
    let auth_manager = Arc::new(AuthManager::new_test_isolated(
        &tmp,
        GrokComConfig::default(),
    ));
    let mgr = ModelsManager::new(
        None,
        catalog,
        acp::ModelId::new("menu-only"),
        auth_manager,
        cfg,
    );
    assert!(mgr.model_supports_reasoning_effort("menu-only"));
    assert_eq!(
        mgr.model_default_reasoning_effort("menu-only"),
        Some(ReasoningEffort::Xhigh)
    );
    assert_eq!(mgr.model_reasoning_efforts("menu-only").len(), 2);
    assert!(!mgr.model_supports_reasoning_effort("plain"));
    assert_eq!(mgr.model_default_reasoning_effort("plain"), None);

    mgr.set_current_model_id(acp::ModelId::new("plain"));
    assert_eq!(mgr.current_model_id().0.as_ref(), "plain");
    assert_eq!(mgr.model_reasoning_efforts("menu-only").len(), 2);
    assert!(mgr.model_reasoning_efforts("plain").is_empty());
}

#[test]
fn cli_reasoning_effort_override_only_stamps_supporting_models() {
    use indexmap::IndexMap;

    let cfg = config::Config {
        reasoning_effort_override: Some(ReasoningEffort::High),
        ..config::Config::default()
    };

    let mut prefetched = IndexMap::new();
    let mut reasoning_entry = ModelEntry {
        info: config::ModelInfo::fallback("reasoning-model"),
        api_key: None,
        env_key: None,
        auth_provider: None,
        platform_oauth_active: false,
        api_base_url: None,
    };
    reasoning_entry.info.supports_reasoning_effort = true;
    prefetched.insert("reasoning-model".to_string(), reasoning_entry);

    let plain_entry = ModelEntry {
        info: config::ModelInfo::fallback("plain-model"),
        api_key: None,
        env_key: None,
        auth_provider: None,
        platform_oauth_active: false,
        api_base_url: None,
    };
    prefetched.insert("plain-model".to_string(), plain_entry);

    let catalog = resolve_model_catalog(&cfg, Some(prefetched));
    assert_eq!(
        catalog["reasoning-model"].info.reasoning_effort,
        Some(ReasoningEffort::High),
        "reasoning-supporting model should be stamped",
    );
    assert_eq!(
        catalog["plain-model"].info.reasoning_effort, None,
        "non-reasoning model must NOT be stamped",
    );
}

#[test]
fn apply_refresh_result_only_updates_etag_on_success() {
    let mgr = test_manager();
    let cfg = config::Config::default();
    mgr.inner.catalog.write().etag = Some("\"old\"".to_string());

    assert!(
        !mgr.apply_refresh_result(&cfg, None, Some("\"new\"".to_string())),
        "failed refresh should report no update"
    );
    assert_eq!(
        mgr.inner.catalog.read().etag.as_deref(),
        Some("\"old\""),
        "etag should remain unchanged when refresh fails"
    );
    assert!(
        mgr.prefetched().is_none(),
        "prefetched models should stay unchanged"
    );
}

fn make_model_entry(model_id: &str) -> ModelEntry {
    ModelEntry {
        info: config::ModelInfo::fallback(model_id),
        api_key: None,
        env_key: None,
        auth_provider: None,
        platform_oauth_active: false,
        api_base_url: None,
    }
}

fn make_prefetched(ids: &[&str]) -> IndexMap<String, ModelEntry> {
    ids.iter()
        .map(|id| (id.to_string(), make_model_entry(id)))
        .collect()
}

// ── startup background refresh ─────────────────────────────────────

#[test]
fn spawn_background_refresh_is_noop_when_real_catalog_present() {
    let mgr = test_manager();
    mgr.inner.catalog.write().has_fetched_real_catalog = true;
    mgr.spawn_background_refresh_inner(/*remote_fetch_enabled*/ true); // must not panic (no tokio::spawn taken)
    assert!(mgr.has_fetched_real_catalog());
}

// Guards the readiness-never-blocks invariant in CI; the e2e proofs are `#[ignore]`.
// current_thread: the post-spawn `!polled` check relies on the task not being
// polled until this test awaits.
#[tokio::test(flavor = "current_thread")]
async fn spawn_background_refresh_never_blocks_on_a_hanging_endpoint() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::Notify;

    // Never resolves; signals the instant the detached task first polls it.
    struct NeverResolvingEndpoint {
        polled: Arc<AtomicBool>,
        dispatched: Arc<Notify>,
    }
    impl ModelsEndpoint for NeverResolvingEndpoint {
        fn fetch_models(
            &self,
            _endpoints: config::EndpointsConfig,
            _auth: Option<GrokAuth>,
            _fetch_auth: ModelFetchAuth,
        ) -> ModelsFetchFuture {
            let polled = self.polled.clone();
            let dispatched = self.dispatched.clone();
            Box::pin(async move {
                polled.store(true, Ordering::SeqCst);
                dispatched.notify_one();
                std::future::pending().await
            })
        }
    }

    let polled = Arc::new(AtomicBool::new(false));
    let dispatched = Arc::new(Notify::new());
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default()));
    let mgr = ModelsManagerBuilder::new(
        None,
        make_prefetched(&["grok-4", "grok-4.5"]),
        acp::ModelId::new("grok-4.5"),
        auth_manager,
        config_from_toml("[models]\ndefault = \"grok-4.5\""),
    )
    .endpoint(Arc::new(NeverResolvingEndpoint {
        polled: polled.clone(),
        dispatched: dispatched.clone(),
    }))
    .cache(test_cache_manager(tmp.path()))
    .build();

    mgr.spawn_background_refresh_inner(/*remote_fetch_enabled*/ true);
    assert!(
        !polled.load(Ordering::SeqCst),
        "fetch ran inline on the readiness path; it must be spawned",
    );

    // Generous failure bound: the dispatch may sit behind a full 5s auth dwell.
    tokio::time::timeout(std::time::Duration::from_secs(30), dispatched.notified())
        .await
        .expect("background refresh was never dispatched");
}

#[tokio::test]
#[serial]
async fn sign_out_clears_catalog_rebuilds_bundled_without_fetching() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct BoomEndpoint {
        calls: Arc<AtomicUsize>,
    }
    impl ModelsEndpoint for BoomEndpoint {
        fn fetch_models(
            &self,
            _endpoints: config::EndpointsConfig,
            _auth: Option<GrokAuth>,
            _fetch_auth: ModelFetchAuth,
        ) -> ModelsFetchFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { None })
        }
    }

    // Unset keys so fetch_auth resolves to Session (the sign-out branch).
    let _no_key = EnvGuard::unset("XAI_API_KEY");
    let _no_legacy_key = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
    let calls = Arc::new(AtomicUsize::new(0));
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default()));
    let mgr = ModelsManagerBuilder::new(
        None,
        make_prefetched(&["grok-4", "grok-4.5"]),
        acp::ModelId::new("grok-4.5"),
        auth_manager,
        config_from_toml("[models]\ndefault = \"grok-4.5\""),
    )
    .endpoint(Arc::new(BoomEndpoint {
        calls: calls.clone(),
    }))
    .cache(test_cache_manager(tmp.path()))
    .build();

    mgr.inner.catalog.write().has_fetched_real_catalog = true;
    mgr.inner.user_selected_model.store(true, Ordering::Relaxed);

    mgr.on_auth_changed().await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "sign-out must skip the doomed Session-auth fetch",
    );
    assert!(
        !mgr.has_fetched_real_catalog(),
        "sign-out must drop the prior identity's real catalog",
    );
    assert!(
        !mgr.inner.user_selected_model.load(Ordering::Relaxed),
        "sign-out must reset the user-pick latch",
    );
    assert!(
        !mgr.models().is_empty(),
        "sign-out must rebuild the bundled default catalog",
    );
    assert_eq!(
        *mgr.inner.catalog_progress.borrow(),
        CatalogProgress::Failed,
        "sign-out publishes an outcome so parked waiters wake",
    );
}

#[test]
fn from_config_without_prefetch_produces_usable_catalog() {
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new_test_isolated(
        tmp.path(),
        GrokComConfig::default(),
    ));
    let cfg = config::Config::default();

    let mgr = ModelsManager::from_config(&cfg, None, auth_manager).unwrap();

    let cat = mgr.inner.catalog.read();
    let catalog = &cat.models;
    assert!(
        !catalog.is_empty(),
        "zero-network boot must produce at least one model in the internal catalog"
    );
    let default = mgr.current_model_id();
    assert!(
        catalog.contains_key(default.0.as_ref()),
        "default model {:?} not in internal catalog: {:?}",
        default,
        catalog.keys().collect::<Vec<_>>()
    );
    drop(cat);
    assert!(
        !mgr.has_fetched_real_catalog(),
        "cold-cache boot must not claim a real catalog"
    );
}

// ── auth-change refresh: has_fetched_real_catalog flag ─────────────

#[test]
fn first_apply_refresh_reselects_default_model() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-3".to_string());

    assert!(!mgr.has_fetched_real_catalog());

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);

    assert!(mgr.has_fetched_real_catalog());
    assert_eq!(mgr.current_model_id().0.as_ref(), "grok-3");
}

#[test]
fn subsequent_apply_refresh_preserves_user_model() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-3".to_string());

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));

    mgr.inner.catalog.write().prefetched = None;
    mgr.inner.catalog.write().etag = None;

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4",
        "user's model selection must survive auth-change refresh"
    );
}

#[test]
fn subsequent_refresh_reselects_when_model_removed() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-3".to_string());

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));

    let prefetched = make_prefetched(&["grok-3", "grok-4.5"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-3",
        "should fall back to config default when current is removed"
    );
}

#[test]
fn failed_refresh_does_not_set_has_fetched_real_catalog() {
    let mgr = test_manager();
    let cfg = config::Config::default();

    mgr.apply_refresh_result(&cfg, None, None);

    assert!(
        !mgr.has_fetched_real_catalog(),
        "failed refresh must not flip has_fetched_real_catalog"
    );
}

// ── apply_config: honor changed preferred model from config ────────

#[test]
fn apply_config_honors_new_preferred_model() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-3".to_string());

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));

    let mut stale_cfg = config::Config::default();
    stale_cfg.models.default = None;
    *mgr.inner.cfg.write() = stale_cfg;

    let mut new_cfg = config::Config::default();
    new_cfg.models.default = Some("grok-3".to_string());
    mgr.apply_config(new_cfg);

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-3",
        "apply_config must honor updated preferred model from config"
    );
}

#[test]
fn apply_config_preserves_current_when_preferred_unchanged() {
    let mgr = test_manager();
    let cfg = config::Config::default();

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);

    mgr.set_current_model_id(acp::ModelId::new("grok-4"));

    let new_cfg = config::Config::default();
    mgr.apply_config(new_cfg);

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4",
        "apply_config must not reset model when preferred hasn't changed"
    );
}

#[test]
fn apply_config_falls_back_when_preferred_not_in_catalog() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-3".to_string());

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);

    mgr.set_current_model_id(acp::ModelId::new("grok-4"));

    let mut new_cfg = config::Config::default();
    new_cfg.models.default = Some("grok-nonexistent".to_string());
    mgr.apply_config(new_cfg);

    let current = mgr.current_model_id();
    let first_available = mgr.available().keys().next().unwrap().clone();
    assert_eq!(
        current.0.as_ref(),
        first_available.0.as_ref(),
        "should fall back to first visible model when preferred not in catalog"
    );
}

#[test]
fn apply_config_both_none_preferred_preserves_current() {
    let mgr = test_manager();
    let cfg = config::Config::default();
    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));
    let new_cfg = config::Config::default();
    mgr.apply_config(new_cfg);

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4",
        "both-None preferred must preserve user's runtime model"
    );
}

#[test]
fn apply_config_old_some_new_none_preserves_current() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-3".to_string());

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);
    assert_eq!(mgr.current_model_id().0.as_ref(), "grok-3");

    mgr.set_current_model_id(acp::ModelId::new("grok-4"));

    let new_cfg = config::Config::default();
    mgr.apply_config(new_cfg);

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4",
        "old=Some new=None must not reset model (is_some guard)"
    );
}

// ── end-to-end: auth refresh + config reload compose correctly ───

#[test]
fn auth_refresh_then_config_reload_preserves_user_model() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-3".to_string());

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);

    mgr.set_current_model_id(acp::ModelId::new("grok-4"));

    mgr.inner.catalog.write().prefetched = None;
    mgr.inner.catalog.write().etag = None;

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);
    assert_eq!(mgr.current_model_id().0.as_ref(), "grok-4");

    let mut new_cfg = config::Config::default();
    new_cfg.models.default = Some("grok-4".to_string());
    mgr.apply_config(new_cfg);
    assert_eq!(mgr.current_model_id().0.as_ref(), "grok-4");
}

// ── disk-cache hot-reload (external models_cache.json writes) ────

fn test_cache_manager(dir: &std::path::Path) -> ModelsCacheManager {
    ModelsCacheManager {
        path: dir.join(MODELS_CACHE_FILE),
        ttl: CACHE_TTL,
    }
}

#[test]
fn reload_from_disk_cache_applies_external_catalog() {
    let mgr = test_manager();
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());

    let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
    cache.persist(
        &make_prefetched(&["grok-4.5", "grok-4.3"]),
        Some("etag-ext"),
        auth_method,
        &mgr.cache_origin(),
    );

    mgr.reload_from_cache_manager(&cache);

    assert!(mgr.has_fetched_real_catalog());
    assert!(mgr.models().contains_key("grok-4.5"));
    assert!(mgr.models().contains_key("grok-4.3"));
    assert_eq!(mgr.inner.catalog.read().etag.as_deref(), Some("etag-ext"));
}

#[test]
fn reload_from_disk_cache_recomputes_allowlist_excludes_all() {
    let mgr = test_manager();
    let cfg = config_from_toml("[models]\nallowed_models = [\"keep-*\"]");

    mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["other-1"])), None);
    assert!(
        mgr.allowlist_excludes_all(),
        "setup: allowlist should exclude the entire catalog"
    );
    *mgr.inner.cfg.write() = cfg.clone();

    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
    cache.persist(
        &make_prefetched(&["keep-1"]),
        Some("etag-keep"),
        auth_method,
        &mgr.cache_origin(),
    );

    mgr.reload_from_cache_manager(&cache);

    assert!(mgr.models().contains_key("keep-1"));
    assert!(
        !mgr.allowlist_excludes_all(),
        "corrective external cache write must unlatch the prompt block"
    );
}

#[test]
fn reload_from_disk_cache_resolves_default_on_first_catalog() {
    let mgr = test_manager();
    assert!(!mgr.has_fetched_real_catalog());
    let cfg = config_from_toml("[models]\ndefault = \"keep-1\"");
    *mgr.inner.cfg.write() = cfg.clone();

    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
    cache.persist(
        &make_prefetched(&["keep-1", "other-1"]),
        Some("etag-first"),
        auth_method,
        &mgr.cache_origin(),
    );

    mgr.reload_from_cache_manager(&cache);

    assert!(mgr.has_fetched_real_catalog());
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "keep-1",
        "first real catalog must resolve the configured default"
    );
}

#[test]
fn reload_from_disk_cache_skips_identical_catalog_and_adopts_etag() {
    let mgr = test_manager();
    let cfg = config::Config::default();
    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched.clone()), Some("etag-a".into()));
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));

    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
    cache.persist(
        &prefetched,
        Some("etag-b"),
        auth_method,
        &mgr.cache_origin(),
    );

    mgr.reload_from_cache_manager(&cache);

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4",
        "identical catalog must not disturb the user's model"
    );
    assert_eq!(
        mgr.inner.catalog.read().etag.as_deref(),
        Some("etag-b"),
        "etag should be adopted so refresh_if_new_etag stays accurate"
    );
}

#[test]
fn reload_from_disk_cache_ignores_stale_cache() {
    let mgr = test_manager();
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
    let stale = ModelsCache {
        fetched_at: Utc::now() - ChronoDuration::seconds(3600),
        grok_version: Some(xai_grok_version::VERSION.to_string()),
        auth_method: Some(auth_method),
        origin: Some(mgr.cache_origin()),
        etag: Some("etag-stale".into()),
        models: make_prefetched(&["grok-stale"]),
    };
    cache.atomic_write(&stale);

    mgr.reload_from_cache_manager(&cache);

    assert!(!mgr.models().contains_key("grok-stale"));
    assert!(mgr.inner.catalog.read().etag.is_none());
}

#[test]
fn reload_from_disk_cache_ignores_auth_method_mismatch() {
    let mgr = test_manager();
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    let current = mgr.inner.fetch_auth.read().cache_auth_method();
    let other = if current == CacheAuthMethod::Session {
        CacheAuthMethod::ApiKey
    } else {
        CacheAuthMethod::Session
    };
    cache.persist(
        &make_prefetched(&["grok-other-auth"]),
        Some("etag-x"),
        other,
        &mgr.cache_origin(),
    );

    mgr.reload_from_cache_manager(&cache);

    assert!(!mgr.models().contains_key("grok-other-auth"));
}

#[test]
fn reload_from_disk_cache_ignores_origin_mismatch() {
    let mgr = test_manager();
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
    cache.persist(
        &make_prefetched(&["grok-other-origin"]),
        Some("etag-y"),
        auth_method,
        "http://127.0.0.1:49953/v1/models",
    );

    mgr.reload_from_cache_manager(&cache);

    assert!(!mgr.models().contains_key("grok-other-origin"));
    assert!(mgr.inner.catalog.read().etag.is_none());
}

#[test]
fn reload_from_disk_cache_ignores_legacy_cache_without_origin() {
    let mgr = test_manager();
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
    let legacy = ModelsCache {
        fetched_at: Utc::now(),
        grok_version: Some(xai_grok_version::VERSION.to_string()),
        auth_method: Some(auth_method),
        origin: None,
        etag: Some("etag-legacy".into()),
        models: make_prefetched(&["grok-legacy"]),
    };
    cache.atomic_write(&legacy);

    mgr.reload_from_cache_manager(&cache);

    assert!(!mgr.models().contains_key("grok-legacy"));
}

// ── clear() resets has_fetched_real_catalog ──────────────────────

#[test]
fn clear_resets_has_fetched_real_catalog() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-3".to_string());

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);
    assert!(mgr.has_fetched_real_catalog());

    mgr.clear();
    assert!(!mgr.has_fetched_real_catalog());

    let prefetched = make_prefetched(&["grok-4.5", "grok-4.3"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);
    let first_available = mgr.available().keys().next().unwrap().clone();
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        first_available.0.as_ref()
    );
}

#[test]
fn is_campaign_only_flip_detects_campaign_driven_changes() {
    let camp: std::collections::HashSet<String> = ["beta".into()].into_iter().collect();
    assert!(is_campaign_only_flip(
        &Some("alpha".into()),
        &Some("beta".into()),
        &camp
    ));
    assert!(is_campaign_only_flip(
        &Some("beta".into()),
        &Some("alpha".into()),
        &camp
    ));
    assert!(!is_campaign_only_flip(
        &Some("alpha".into()),
        &Some("gamma".into()),
        &camp
    ));
    assert!(!is_campaign_only_flip(
        &Some("beta".into()),
        &Some("beta".into()),
        &camp
    ));
    assert!(!is_campaign_only_flip(&Some("beta".into()), &None, &camp));
    assert!(!is_campaign_only_flip(
        &Some("alpha".into()),
        &Some("beta".into()),
        &std::collections::HashSet::new()
    ));
}

#[test]
fn campaign_only_flip_does_not_reselect_live_session() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("alpha".to_string());
    mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["alpha", "beta"])), None);
    *mgr.inner.cfg.write() = cfg.clone(); // old_preferred = "alpha"
    assert_eq!(mgr.current_model_id().0.as_ref(), "alpha");

    let mut new_cfg = config::Config::default();
    new_cfg.models.default = Some("beta".to_string());
    new_cfg.models.default_is_campaign_driven = true; // campaign overriding
    mgr.apply_config(new_cfg);
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "alpha",
        "campaign-only flip must not yank a still-selectable live session"
    );

    let mgr2 = test_manager();
    let mut cfg2 = config::Config::default();
    cfg2.models.default = Some("alpha".to_string());
    mgr2.apply_refresh_result(&cfg2, Some(make_prefetched(&["alpha", "beta"])), None);
    *mgr2.inner.cfg.write() = cfg2.clone();
    let mut new_cfg2 = config::Config::default();
    new_cfg2.models.default = Some("beta".to_string());
    mgr2.apply_config(new_cfg2);
    assert_eq!(
        mgr2.current_model_id().0.as_ref(),
        "beta",
        "a non-campaign preferred change must reselect"
    );
}

#[test]
fn unavailable_campaign_default_falls_back_to_config_default() {
    let catalog = make_prefetched(&["real-model", "other-model"]);

    let mut cfg = config::Config::default();
    cfg.models.default = Some("missing-model".to_string());
    cfg.models.default_is_campaign_driven = true;
    cfg.models.pre_campaign_default = Some("real-model".to_string());
    let (key, _, _) = resolve_default_model(&cfg, &catalog, true);
    assert_eq!(
        key, "real-model",
        "must fall back to the pre-campaign default"
    );

    let mut cfg2 = config::Config::default();
    cfg2.models.default = Some("missing-model".to_string());
    cfg2.models.default_is_campaign_driven = true;
    cfg2.models.pre_campaign_default = Some("also-missing".to_string());
    let (key2, _, _) = resolve_default_model(&cfg2, &catalog, true);
    assert_eq!(&key2, catalog.keys().next().unwrap());

    let mut cfg3 = config::Config::default();
    cfg3.models.default = Some("missing-model".to_string());
    cfg3.models.pre_campaign_default = Some("real-model".to_string());
    let (key3, _, _) = resolve_default_model(&cfg3, &catalog, true);
    assert_eq!(
        &key3,
        catalog.keys().next().unwrap(),
        "non-campaign catalog miss must not recover via campaign state"
    );

    let mut cfg4 = config::Config {
        default_model_override: Some("missing-cli-model".to_string()),
        ..Default::default()
    };
    cfg4.models.default = Some("campaign-model".to_string());
    cfg4.models.default_is_campaign_driven = true;
    cfg4.models.pre_campaign_default = Some("real-model".to_string());
    let (key4, _, _) = resolve_default_model(&cfg4, &catalog, true);
    assert_eq!(
        &key4,
        catalog.keys().next().unwrap(),
        "a CLI pref miss must not detour through pre_campaign_default"
    );
}

// ── ModelFetchAuth::resolve priority tests ──────────────────────

use serial_test::serial;
use xai_grok_test_support::EnvGuard;

#[test]
#[serial]
fn resolve_custom_endpoint_always_wins() {
    let _key = EnvGuard::set("XAI_API_KEY", "test-key");
    let endpoints = config::EndpointsConfig {
        models_base_url: Some("https://custom.example.com".to_owned()),
        ..config::EndpointsConfig::default()
    };
    assert_eq!(
        ModelFetchAuth::resolve(&endpoints, true),
        ModelFetchAuth::CustomEndpoint,
    );
    assert_eq!(
        ModelFetchAuth::resolve(&endpoints, false),
        ModelFetchAuth::CustomEndpoint,
    );
}

#[test]
#[serial]
fn resolve_cached_session_wins_over_api_key() {
    let _key = EnvGuard::set("XAI_API_KEY", "test-key");
    let endpoints = config::EndpointsConfig::default();
    assert_eq!(
        ModelFetchAuth::resolve(&endpoints, true),
        ModelFetchAuth::Session,
        "cached session should take priority over API key",
    );
}

#[test]
#[serial]
fn resolve_api_key_used_when_no_session() {
    let _key = EnvGuard::set("XAI_API_KEY", "test-key");
    let endpoints = config::EndpointsConfig::default();
    assert_eq!(
        ModelFetchAuth::resolve(&endpoints, false),
        ModelFetchAuth::ApiKey,
        "API key should be used when no cached session exists",
    );
}

#[test]
#[serial]
fn resolve_falls_back_to_session_when_nothing_set() {
    let _unset = EnvGuard::unset("XAI_API_KEY");
    let _unset_legacy = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
    let endpoints = config::EndpointsConfig::default();
    assert_eq!(
        ModelFetchAuth::resolve(&endpoints, false),
        ModelFetchAuth::Session,
        "should fall back to Session when nothing else is configured",
    );
}

#[test]
#[serial]
fn resolve_deployment_key_when_no_session_or_api_key() {
    let _unset = EnvGuard::unset("XAI_API_KEY");
    let _unset_legacy = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
    let endpoints = config::EndpointsConfig {
        deployment_key: Some("deploy-key".to_owned()),
        ..config::EndpointsConfig::default()
    };
    assert_eq!(
        ModelFetchAuth::resolve(&endpoints, false),
        ModelFetchAuth::Deployment,
    );
}

#[test]
#[serial]
fn resolve_deployment_key_outranks_ambient_api_key() {
    let _key = EnvGuard::set("XAI_API_KEY", "stray-env-key");
    let endpoints = config::EndpointsConfig {
        deployment_key: Some("deploy-key".to_owned()),
        ..config::EndpointsConfig::default()
    };
    assert_eq!(
        ModelFetchAuth::resolve(&endpoints, false),
        ModelFetchAuth::Deployment,
        "managed deployment_key should outrank an ambient XAI_API_KEY",
    );
    assert_eq!(
        ModelFetchAuth::resolve(&endpoints, true),
        ModelFetchAuth::Session,
        "an active session should still win over a managed deployment",
    );
}

// ── remote_fetch gate: resolve_prefetch_env_from_parts ───────────

#[test]
#[serial]
fn prefetch_env_none_when_remote_fetch_disabled_despite_credentials() {
    let _key = EnvGuard::set("XAI_API_KEY", "stray-env-key");
    let endpoints = config::EndpointsConfig {
        deployment_key: Some("deploy-key".to_owned()),
        models_base_url: Some("https://custom.example.com".to_owned()),
        ..config::EndpointsConfig::default()
    };
    assert!(
        resolve_prefetch_env_from_parts(Some(GrokAuth::test_default()), endpoints.clone(), false,)
            .is_none(),
        "session auth must not re-arm the prefetch when remote_fetch is off",
    );
    assert!(
        resolve_prefetch_env_from_parts(None, endpoints, false).is_none(),
        "API key / deployment key / custom endpoint must not re-arm it either",
    );
}

#[test]
#[serial]
fn prefetch_env_resolves_when_remote_fetch_enabled() {
    let _unset = EnvGuard::unset("XAI_API_KEY");
    let _unset_legacy = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
    let (_dir, _platform_guards) = isolate_platform_credentials();
    let endpoints = config::EndpointsConfig {
        deployment_key: Some("deploy-key".to_owned()),
        ..config::EndpointsConfig::default()
    };
    assert!(resolve_prefetch_env_from_parts(None, endpoints, true).is_some());
    assert!(
        resolve_prefetch_env_from_parts(None, config::EndpointsConfig::default(), true).is_none(),
        "no credentials and no custom endpoint must stay a no-prefetch launch",
    );
}

#[tokio::test]
async fn fetch_and_apply_degrades_offline_when_remote_fetch_disabled() {
    let mgr = test_manager();
    mgr.insert_test_entry(
        "static-one",
        ModelEntry {
            info: config::ModelInfo::fallback("static-one"),
            api_key: None,
            env_key: None,
            auth_provider: None,
            platform_oauth_active: false,
            api_base_url: None,
        },
    );

    mgr.fetch_and_apply_inner(false).await;

    assert!(
        !mgr.has_fetched_real_catalog(),
        "no catalog fetch may be recorded when remote_fetch is disabled",
    );
    assert!(
        mgr.models().contains_key("static-one"),
        "the static catalog must keep resolving",
    );
}

// ── supported_in_api tests ──────────────────────────────────────

#[test]
fn default_model_skips_oauth_only_for_api_key_users() {
    let cfg = config::Config::default();
    let mut catalog = IndexMap::new();

    let mut oauth_only = ModelEntry {
        info: config::ModelInfo::fallback("oauth-only"),
        api_key: None,
        env_key: None,
        auth_provider: None,
        platform_oauth_active: false,
        api_base_url: None,
    };
    oauth_only.info.supported_in_api = false;
    catalog.insert("oauth-only".to_string(), oauth_only);

    let public = ModelEntry {
        info: config::ModelInfo::fallback("public-model"),
        api_key: None,
        env_key: None,
        auth_provider: None,
        platform_oauth_active: false,
        api_base_url: None,
    };
    catalog.insert("public-model".to_string(), public);

    let (key, _, _) = resolve_default_model(&cfg, &catalog, false);
    assert_ne!(
        key, "oauth-only",
        "API-key default must not be an OAuth-only model"
    );
    assert_eq!(key, "public-model");

    let (key, _, _) = resolve_default_model(&cfg, &catalog, true);
    assert!(
        key == "oauth-only" || key == "public-model",
        "OAuth user should be able to use either model as default"
    );
}

#[test]
fn visible_for_auth_logic() {
    let mut info = config::ModelInfo::fallback("test");

    assert!(info.visible_for_auth(true));
    assert!(info.visible_for_auth(false));

    info.hidden = true;
    assert!(!info.visible_for_auth(true));
    assert!(!info.visible_for_auth(false));

    info.hidden = false;
    info.supported_in_api = false;
    assert!(info.visible_for_auth(true));
    assert!(!info.visible_for_auth(false));
}

#[test]
fn managed_platform_models_hidden_without_credentials_even_for_session() {
    let mut entry = ModelEntry {
        info: config::ModelInfo::fallback("gpt-5"),
        api_key: None,
        env_key: Some(config::EnvKeys::new(["OPENAI_API_KEY_MUST_NOT_EXIST_XYZ"])),
        auth_provider: None,
        platform_oauth_active: false,
        api_base_url: None,
    };
    entry.info.id = Some("openai/gpt-5".into());
    entry.info.supported_in_api = false;
    assert!(entry.is_managed_platform_model());
    assert!(
        !entry.visible_for_auth(true),
        "xAI session must not show openai/* without an API key"
    );
    assert!(!entry.visible_for_auth(false));

    entry.api_key = Some("sk-test".into());
    assert!(
        entry.visible_for_auth(true),
        "stamped API key makes the platform model selectable"
    );
    assert!(entry.visible_for_auth(false));
}

// ── duplicate model slug re-keying (A/B experiment "auto" alias) ──

fn make_entry_config(model: &str, name: Option<&str>) -> config::ModelEntryConfig {
    make_entry_config_with_id(None, model, name)
}

fn make_entry_config_with_id(
    id: Option<&str>,
    model: &str,
    name: Option<&str>,
) -> config::ModelEntryConfig {
    config::ModelEntryConfig {
        id: id.map(|s| s.to_owned()),
        model_family: None,
        model: model.to_owned(),
        base_url: "https://test.api/v1".to_owned(),
        name: name.map(|n| n.to_owned()),
        description: None,
        max_completion_tokens: None,
        temperature: None,
        top_p: None,
        api_key: None,
        env_key: None,
        api_backend: Default::default(),
        request_compat: None,
        endpoint_path: None,
        context_window: std::num::NonZeroU64::new(200_000).unwrap(),
        auto_compact_threshold_percent: None,
        system_prompt_label: None,
        extra_headers: IndexMap::new(),
        query_params: IndexMap::new(),
        api_base_url: None,
        use_concise: false,
        agent_type: config::default_agent_type(),
        inference_idle_timeout_secs: None,
        max_retries: None,
        subagent_rate_limit_max_attempts: None,
        hidden: false,
        supported_in_api: true,
        auth_scheme: None,
        reasoning_effort: None,
        supports_reasoning_effort: false,
        reasoning_efforts: Vec::new(),
        supports_backend_search: false,
        compactions_remaining: None,
        compaction_at_tokens: None,
        show_model_fingerprint: false,
        stream_tool_calls: None,
        laziness_detector: config::LazinessDetectorPerModelConfig::default(),
    }
}

#[test]
fn build_prefetched_map_distinct_ids_same_slug() {
    let entries = vec![
        make_entry_config_with_id(Some("auto"), "grok-build", Some("Auto")),
        make_entry_config_with_id(Some("grok-build"), "grok-build", Some("Grok Build")),
        make_entry_config_with_id(
            Some("experimental-fast"),
            "experimental-fast",
            Some("Grok Fast"),
        ),
    ];
    let map = build_prefetched_map(entries, None);

    assert_eq!(map.len(), 3, "all three entries should survive");
    assert!(map.contains_key("auto"));
    assert!(map.contains_key("grok-build"));
    assert!(map.contains_key("experimental-fast"));
    assert_eq!(
        map["auto"].info.model, "grok-build",
        "auto entry should still route to grok-build"
    );
    assert_eq!(map["grok-build"].info.model, "grok-build");
}

#[test]
fn build_prefetched_map_no_id_falls_back_to_slug() {
    let entries = vec![
        make_entry_config("model-a", Some("Model A")),
        make_entry_config("model-b", Some("Model B")),
    ];
    let map = build_prefetched_map(entries, None);

    assert_eq!(map.len(), 2);
    assert!(map.contains_key("model-a"));
    assert!(map.contains_key("model-b"));
}

#[test]
fn build_prefetched_map_duplicate_id_overwrites() {
    let entries = vec![
        make_entry_config_with_id(Some("grok-build"), "grok-build", Some("First")),
        make_entry_config_with_id(Some("grok-build"), "grok-build", Some("Second")),
    ];
    let map = build_prefetched_map(entries, None);

    assert_eq!(map.len(), 1, "duplicate id: second overwrites first");
    assert_eq!(map["grok-build"].info.name.as_deref(), Some("Second"));
}

#[test]
fn resolve_default_model_prefers_id_over_model_slug() {
    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    catalog.insert(
        "auto-grok-build".to_string(),
        make_model_entry("grok-build"),
    );
    catalog.insert("grok-build".to_string(), make_model_entry("grok-build"));

    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-build".to_string());

    let (key, _, _) = resolve_default_model(&cfg, &catalog, true);
    assert_eq!(key, "grok-build", "must match id, not first slug hit");
}

#[test]
fn build_prefetched_map_none_id_falls_back_to_slug() {
    let entries = vec![make_entry_config_with_id(
        None,
        "grok-build",
        Some("Grok Build"),
    )];
    let map = build_prefetched_map(entries, None);

    assert_eq!(map.len(), 1);
    assert!(map.contains_key("grok-build"));
}

// ── persisted model id → catalog key (session resume) ─────────────

#[test]
fn resolve_catalog_key_maps_routing_slug_to_config_key() {
    let mut models = IndexMap::new();
    models.insert(
        "enterprise-grok-build".to_string(),
        make_model_entry("grok-4.5"),
    );
    models.insert("grok-4.3".to_string(), make_model_entry("grok-4.3"));

    let persisted = acp::ModelId::new("grok-4.5");
    let key = resolve_catalog_key(&models, &persisted).expect("slug must resolve");
    assert_eq!(key.0.as_ref(), "enterprise-grok-build");
}

#[test]
fn resolve_catalog_key_prefers_exact_key_match() {
    let mut models = IndexMap::new();
    models.insert("grok-4.5".to_string(), make_model_entry("grok-4.5"));

    let persisted = acp::ModelId::new("grok-4.5");
    let key = resolve_catalog_key(&models, &persisted).expect("exact key must resolve");
    assert_eq!(key.0.as_ref(), "grok-4.5");
}

#[test]
fn resolve_catalog_key_last_slug_match_wins() {
    let mut models = IndexMap::new();
    models.insert(
        "default-grok-build".to_string(),
        make_model_entry("grok-4.5"),
    );
    models.insert("user-grok-build".to_string(), make_model_entry("grok-4.5"));

    let persisted = acp::ModelId::new("grok-4.5");
    let key = resolve_catalog_key(&models, &persisted).expect("slug must resolve");
    assert_eq!(key.0.as_ref(), "user-grok-build");
}

#[test]
fn selectable_catalog_key_for_persisted_none_when_resolved_not_available() {
    let mut models = IndexMap::new();
    models.insert(
        "enterprise-grok-build".to_string(),
        make_model_entry("grok-4.5"),
    );

    let available: IndexMap<_, _> = IndexMap::new();
    let persisted = acp::ModelId::new("grok-4.5");
    assert!(selectable_catalog_key_for_persisted(&models, &available, &persisted).is_none());
}

#[test]
fn selectable_prefers_available_identity_over_non_selectable_exact_key() {
    let mut models = IndexMap::new();
    models.insert("grok-build".to_string(), make_model_entry("grok-build"));
    models.insert(
        "enterprise-grok-build".to_string(),
        make_model_entry("grok-build"),
    );
    models.insert("grok-4.3".to_string(), make_model_entry("grok-4.3"));

    let available = test_available_keys(&["enterprise-grok-build", "grok-4.3"]);

    let persisted = acp::ModelId::new("grok-build");
    assert_eq!(
        resolve_catalog_key(&models, &persisted)
            .expect("exact key exists")
            .0
            .as_ref(),
        "grok-build"
    );
    let key = selectable_catalog_key_for_persisted(&models, &available, &persisted)
        .expect("must resolve to selectable section");
    assert_eq!(key.0.as_ref(), "enterprise-grok-build");
}

#[test]
fn selectable_matches_routing_slug_when_no_exact_key() {
    let mut models = IndexMap::new();
    models.insert(
        "enterprise-grok-build".to_string(),
        make_model_entry("grok-build"),
    );
    models.insert("grok-4.3".to_string(), make_model_entry("grok-4.3"));

    let available = test_available_keys(&["enterprise-grok-build", "grok-4.3"]);

    let persisted = acp::ModelId::new("grok-build");
    let key = selectable_catalog_key_for_persisted(&models, &available, &persisted)
        .expect("slug must resolve to selectable key");
    assert_eq!(key.0.as_ref(), "enterprise-grok-build");
}

#[test]
fn selectable_prefers_exact_key_over_later_slug_match() {
    let mut models = IndexMap::new();
    models.insert("grok-build".to_string(), make_model_entry("grok-4.5"));
    models.insert("other".to_string(), make_model_entry("grok-build"));

    let available = test_available_keys(&["grok-build", "other"]);

    let persisted = acp::ModelId::new("grok-build");
    let key = selectable_catalog_key_for_persisted(&models, &available, &persisted)
        .expect("exact selectable key must win");
    assert_eq!(key.0.as_ref(), "grok-build");
}

#[test]
fn selectable_catalog_key_for_persisted_skips_locked_platform_models() {
    let mut models = IndexMap::new();
    models.insert(
        "deepseek/deepseek-v4-flash".to_string(),
        make_model_entry("deepseek-v4-flash"),
    );
    models.insert("grok-4.5".to_string(), make_model_entry("grok-4.5"));

    let mut available = test_available_keys(&["grok-4.5"]);
    let locked_id = acp::ModelId::new("deepseek/deepseek-v4-flash");
    let lock_meta = serde_json::json!({
        "requiresApiKey": true,
        "platform": "deepseek",
        "platformName": "DeepSeek",
        "apiKeyEnv": ["GROK_DEEPSEEK_API_KEY", "DEEPSEEK_API_KEY"],
        "setupHint": "export DEEPSEEK_API_KEY=…",
    })
    .as_object()
    .cloned();
    available.insert(
        locked_id.clone(),
        acp::ModelInfo::new(locked_id.clone(), "DeepSeek V4 Flash".to_string()).meta(lock_meta),
    );

    let persisted = acp::ModelId::new("deepseek/deepseek-v4-flash");
    assert!(
        selectable_catalog_key_for_persisted(&models, &available, &persisted).is_none(),
        "locked platform model must not count as selectable for restore"
    );
    let usable = acp::ModelId::new("grok-4.5");
    assert_eq!(
        selectable_catalog_key_for_persisted(&models, &available, &usable),
        Some(usable)
    );
}

fn test_available_keys(keys: &[&str]) -> IndexMap<acp::ModelId, acp::ModelInfo> {
    keys.iter()
        .map(|k| {
            let id = acp::ModelId::new(*k);
            (id.clone(), acp::ModelInfo::new(id, (*k).to_string()))
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn bounded_auth_refresh_times_out_to_none() {
    // A hung IdP (never-ready auth future) must degrade to None within the
    // bound so a cold-cache boot fetch can't stall on it.
    let started = tokio::time::Instant::now();
    let result =
        ModelsManager::bounded_auth_refresh(std::future::pending::<Option<GrokAuth>>()).await;
    assert!(result.is_none(), "a hung auth refresh must yield None");
    assert!(
        started.elapsed() >= crate::http::STARTUP_AUTH_REFRESH_TIMEOUT,
        "must wait the full bound before giving up",
    );
}

#[tokio::test]
async fn bounded_auth_refresh_passes_through_ready_value() {
    let result =
        ModelsManager::bounded_auth_refresh(async { Some(GrokAuth::test_default()) }).await;
    assert!(
        result.is_some(),
        "a ready session must pass through unchanged"
    );
}

#[tokio::test]
async fn explicit_model_pick_survives_first_real_catalog() {
    // Non-blocking boot lets the user pick a model before the first real
    // catalog lands; that pick must not be clobbered by default reselection.
    let mgr = test_manager();
    let cfg = config_from_toml("[models]\ndefault = \"grok-4.5\"");
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));
    mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["grok-4.5", "grok-4"])), None);
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4",
        "an explicit /model pick must survive the first real catalog",
    );
}

#[tokio::test]
async fn identity_switch_clears_user_pick_latch() {
    // After an identity change (`clear()`), the new identity's first catalog must
    // reselect its own default rather than inherit the prior user's pick.
    let mgr = test_manager();
    let cfg = config_from_toml("[models]\ndefault = \"grok-4.5\"");
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));
    mgr.clear();
    mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["grok-4.5", "grok-4"])), None);
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4.5",
        "a new identity's first catalog must reselect the default after clear()",
    );
}

// ── Isolation: explicit path injection (no process-shared cache) ────

/// Production default may resolve via grok_home OnceLock; unit tests must
/// always inject a per-test TempDir via `with_path` / `test_cache_manager`.
#[test]
fn models_cache_manager_with_path_is_isolated_per_tempdir() {
    let a = tempfile::TempDir::new().unwrap();
    let b = tempfile::TempDir::new().unwrap();
    let ma = ModelsCacheManager::with_path(a.path().join(MODELS_CACHE_FILE));
    let mb = ModelsCacheManager::with_path(b.path().join(MODELS_CACHE_FILE));
    assert_ne!(ma.path, mb.path);
    assert!(ma.path.starts_with(a.path()));
    assert!(mb.path.starts_with(b.path()));
    // Neither path is under the real user home models cache.
    #[allow(deprecated)]
    let real = std::env::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".grok")
        .join(MODELS_CACHE_FILE);
    assert_ne!(ma.path, real);
    assert_ne!(mb.path, real);
}

/// Non-empty `GROK_MODELS_CACHE_PATH` redirects `models_cache_path()`;
/// empty string is treated as unset (harness convention).
#[test]
#[serial_test::serial]
fn models_cache_path_env_override_and_empty_is_unset() {
    use xai_grok_test_support::EnvGuard;
    let dir = tempfile::tempdir().unwrap();
    let override_path = dir.path().join("custom-models-cache.json");
    {
        let _g = EnvGuard::set("GROK_MODELS_CACHE_PATH", override_path.to_str().unwrap());
        assert_eq!(models_cache_path(), override_path);
    }
    {
        let _g = EnvGuard::set("GROK_MODELS_CACHE_PATH", "");
        // Empty = unset → must not equal the empty-path join; resolves via grok_home.
        let resolved = models_cache_path();
        assert_ne!(resolved, std::path::PathBuf::from(""));
        assert!(
            resolved.ends_with(MODELS_CACHE_FILE),
            "empty override falls back to …/models_cache.json, got {}",
            resolved.display()
        );
    }
}

/// Prefetch path accepts an injected cache manager — tests must not call
/// `ModelsCacheManager::new()` (which may resolve real grok_home).
#[test]
fn prefetch_with_cache_uses_injected_path_only() {
    let dir = tempfile::TempDir::new().unwrap();
    let cache_path = dir.path().join(MODELS_CACHE_FILE);
    let cache = ModelsCacheManager::with_path(cache_path.clone());
    // No network: remote_fetch disabled → cache miss returns None without
    // writing outside the injected path.
    let endpoints = crate::agent::config::EndpointsConfig::default();
    let out = super::prefetch_models_blocking_with_cache(
        &endpoints,
        None,
        ModelFetchAuth::Session,
        false,
        cache,
    );
    assert!(out.is_none());
    assert!(
        !cache_path.exists() || cache_path.metadata().map(|m| m.len()).unwrap_or(0) == 0,
        "disabled remote fetch must not poison unrelated paths"
    );
    // Sibling path under real home must not have been created by this call.
    #[allow(deprecated)]
    let real = std::env::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".grok")
        .join(MODELS_CACHE_FILE);
    // We cannot assert real doesn't exist (developer may have it); only that
    // our injected path is the one the manager was bound to.
    let mgr = ModelsCacheManager::with_path(cache_path.clone());
    assert_eq!(mgr.path, cache_path);
    let _ = real; // documented debt: production new() still uses grok_home
}
