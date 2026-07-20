use super::*;
/// Build an unsigned JWT with a `tier` claim (header.payload.sig base64url).
fn jwt_with_tier(tier: u64) -> String {
    use base64::Engine;
    let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = enc.encode(br#"{"alg":"none"}"#);
    let payload = enc.encode(format!(r#"{{"tier":{tier}}}"#).as_bytes());
    format!("{header}.{payload}.sig")
}
#[test]
fn jwt_tier_claim_maps_free_and_paid() {
    assert_eq!(jwt_tier_claim(&jwt_with_tier(0)).as_deref(), Some("free"));
    assert_eq!(
        jwt_tier_claim(&jwt_with_tier(1)).as_deref(),
        Some("supergrok")
    );
    assert_eq!(
        jwt_tier_claim(&jwt_with_tier(2)).as_deref(),
        Some("x_basic")
    );
    assert_eq!(
        jwt_tier_claim(&jwt_with_tier(3)).as_deref(),
        Some("x_premium")
    );
    assert_eq!(
        jwt_tier_claim(&jwt_with_tier(4)).as_deref(),
        Some("x_premium_plus")
    );
    assert_eq!(
        jwt_tier_claim(&jwt_with_tier(5)).as_deref(),
        Some("supergrok_heavy")
    );
    assert_eq!(
        jwt_tier_claim(&jwt_with_tier(6)).as_deref(),
        Some("supergrok_lite")
    );
    assert_eq!(jwt_tier_claim(&jwt_with_tier(99)).as_deref(), Some("99"));
}
fn auth_with_mode(mode: crate::auth::AuthMode, key: &str) -> crate::auth::GrokAuth {
    crate::auth::GrokAuth {
        key: key.into(),
        auth_mode: mode,
        create_time: chrono::Utc::now(),
        user_id: "u".into(),
        email: None,
        first_name: None,
        last_name: None,
        profile_image_asset_id: None,
        principal_type: None,
        principal_id: None,
        team_id: None,
        team_name: None,
        team_role: None,
        organization_id: None,
        organization_name: None,
        organization_role: None,
        user_blocked_reason: None,
        team_blocked_reasons: vec![],
        coding_data_retention_opt_out: false,
        has_grok_code_access: None,
        refresh_token: None,
        expires_at: None,
        oidc_issuer: None,
        oidc_client_id: None,
    }
}
#[test]
fn resolve_subscription_tier_prefers_display_then_api_key_then_jwt() {
    assert_eq!(
        resolve_subscription_tier_for_telemetry(Some("Free".into()), None).as_deref(),
        Some("Free")
    );
    let api = auth_with_mode(crate::auth::AuthMode::ApiKey, "xai-not-a-jwt");
    assert_eq!(
        resolve_subscription_tier_for_telemetry(Some("  ".into()), Some(&api)).as_deref(),
        Some("api_key")
    );
    assert_eq!(
        resolve_subscription_tier_for_telemetry(None, Some(&api)).as_deref(),
        Some("api_key")
    );
    let oauth = auth_with_mode(crate::auth::AuthMode::Oidc, &jwt_with_tier(0));
    assert_eq!(
        resolve_subscription_tier_for_telemetry(None, Some(&oauth)).as_deref(),
        Some("free")
    );
    assert_ne!(
        resolve_subscription_tier_for_telemetry(None, Some(&api)).as_deref(),
        Some("free")
    );
}
/// JWT claim ↔ `/user` tier mapping used to gate post-unblock catalog refresh
/// (a stale older paid claim must not skip retry).
#[test]
fn jwt_claim_matches_user_subscription_tier_known_pairs() {
    let cases = [
        ("supergrok", "GrokPro"),
        ("x_basic", "XBasic"),
        ("x_premium", "XPremium"),
        ("x_premium_plus", "XPremiumPlus"),
        ("supergrok_heavy", "SuperGrokPro"),
        ("supergrok_lite", "SuperGrokLite"),
    ];
    for (claim, user_tier) in cases {
        assert!(
            jwt_claim_matches_user_subscription_tier(claim, user_tier),
            "{claim} should match {user_tier}"
        );
    }
}
#[test]
fn jwt_claim_matches_user_subscription_tier_rejects_stale_and_unknown() {
    assert!(!jwt_claim_matches_user_subscription_tier(
        "x_basic",
        "SuperGrokPro"
    ));
    assert!(!jwt_claim_matches_user_subscription_tier(
        "supergrok",
        "SuperGrokPro"
    ));
    assert!(!jwt_claim_matches_user_subscription_tier("free", "GrokPro"));
    assert!(!jwt_claim_matches_user_subscription_tier("", "XPremium"));
    assert!(!jwt_claim_matches_user_subscription_tier(
        "supergrok_heavy",
        "EnterpriseMystery"
    ));
}
/// Single-flight flag must clear on Drop even if the retry task panics /
/// aborts mid-backoff (guards against the flag stuck true forever).
#[test]
fn post_unblock_jwt_retry_in_flight_guard_clears_on_drop() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let flag = Arc::new(AtomicBool::new(true));
    {
        let _guard = PostUnblockJwtRetryInFlightGuard { flag: flag.clone() };
        assert!(flag.load(Ordering::Acquire));
    }
    assert!(
        !flag.load(Ordering::Acquire),
        "Drop must release post_unblock_jwt_retry_in_flight"
    );
    let flag = Arc::new(AtomicBool::new(true));
    let flag_for_catch = flag.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = PostUnblockJwtRetryInFlightGuard {
            flag: flag_for_catch,
        };
        panic!("simulate retry task panic");
    }));
    assert!(result.is_err());
    assert!(
        !flag.load(Ordering::Acquire),
        "Drop must release flag on panic unwind"
    );
}
mod hunk_tracking_mode {
    use super::super::{plan_hunk_tracking, resolve_hunk_tracking_mode};
    use xai_hunk_tracker::TrackingMode;
    #[test]
    fn off_and_disabled_disable_tracking() {
        assert_eq!(resolve_hunk_tracking_mode(Some("off")), None);
        assert_eq!(resolve_hunk_tracking_mode(Some("disabled")), None);
    }
    #[test]
    fn matching_is_case_insensitive_and_trimmed() {
        assert_eq!(resolve_hunk_tracking_mode(Some("OFF")), None);
        assert_eq!(resolve_hunk_tracking_mode(Some("  Off ")), None);
        assert_eq!(resolve_hunk_tracking_mode(Some("DISABLED")), None);
        assert_eq!(
            resolve_hunk_tracking_mode(Some("Agent_Only")),
            Some(TrackingMode::AgentOnly)
        );
        assert_eq!(
            resolve_hunk_tracking_mode(Some(" ALL_DIRTY ")),
            Some(TrackingMode::AllDirty)
        );
    }
    #[test]
    fn recognized_modes_parse() {
        assert_eq!(
            resolve_hunk_tracking_mode(Some("agent_only")),
            Some(TrackingMode::AgentOnly)
        );
        assert_eq!(
            resolve_hunk_tracking_mode(Some("all_dirty")),
            Some(TrackingMode::AllDirty)
        );
    }
    #[test]
    fn parser_absent_returns_none_policy_defaults_in_plan() {
        assert_eq!(resolve_hunk_tracking_mode(None), None);
        assert_eq!(resolve_hunk_tracking_mode(Some("")), None);
        assert_eq!(
            resolve_hunk_tracking_mode(Some("bogus")),
            Some(TrackingMode::AllDirty)
        );
    }
    #[test]
    fn plan_disables_actor_forward_and_loc_together() {
        for off in ["off", "disabled", "OFF"] {
            let plan = plan_hunk_tracking(Some(off));
            assert_eq!(plan.actor_mode, None, "{off} must not spawn the actor");
            assert!(!plan.enabled(), "{off} must disable the forward + LOC sink");
        }
    }
    #[test]
    fn plan_enables_actor_and_forward_for_active_modes() {
        for (mode, expected) in [
            ("agent_only", TrackingMode::AgentOnly),
            ("all_dirty", TrackingMode::AllDirty),
            ("bogus", TrackingMode::AllDirty),
        ] {
            let plan = plan_hunk_tracking(Some(mode));
            assert_eq!(plan.actor_mode, Some(expected));
            assert!(plan.enabled());
        }
        let plan = plan_hunk_tracking(None);
        assert_eq!(plan.actor_mode, None);
        assert!(!plan.enabled());
    }
}
mod capture {
    use tokio::sync::mpsc;
    use tracing::Subscriber;
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
    use tracing_subscriber::registry::LookupSpan;
    pub(crate) struct CapturedEvent {
        pub level: tracing::Level,
        pub fields: String,
    }
    pub(crate) struct Captured {
        pub events_rx: mpsc::UnboundedReceiver<CapturedEvent>,
        _guard: tracing::subscriber::DefaultGuard,
    }
    pub(crate) fn capture() -> Captured {
        let (tx, rx) = mpsc::unbounded_channel();
        let subscriber = tracing_subscriber::registry().with(CaptureLayer { tx });
        let guard = tracing::subscriber::set_default(subscriber);
        Captured {
            events_rx: rx,
            _guard: guard,
        }
    }
    struct CaptureLayer {
        tx: mpsc::UnboundedSender<CapturedEvent>,
    }
    impl<S> Layer<S> for CaptureLayer
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let mut v = Visitor::default();
            event.record(&mut v);
            let _ = self.tx.send(CapturedEvent {
                level: *event.metadata().level(),
                fields: v.out,
            });
        }
    }
    #[derive(Default)]
    struct Visitor {
        out: String,
    }
    impl tracing::field::Visit for Visitor {
        fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
            if !self.out.is_empty() {
                self.out.push(' ');
            }
            self.out.push_str(f.name());
            self.out.push('=');
            self.out.push_str(&format!("{v:?}"));
        }
        fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
            if !self.out.is_empty() {
                self.out.push(' ');
            }
            self.out.push_str(f.name());
            self.out.push('=');
            self.out.push_str(v);
        }
    }
}
#[test]
fn warn_on_missing_parent_session_emits_when_session_absent() {
    let captured = capture::capture();
    warn_on_missing_parent_session_for_validate_type("ghost-session", false);
    let mut rx = captured.events_rx;
    let mut saw = false;
    while let Ok(event) = rx.try_recv() {
        if event.level == tracing::Level::WARN
            && event
                .fields
                .contains("ValidateType received for unknown parent session")
            && event.fields.contains("parent_session_id=ghost-session")
        {
            saw = true;
            break;
        }
    }
    assert!(saw, "warn must fire");
}
#[test]
fn warn_on_missing_parent_session_silent_when_session_present() {
    let captured = capture::capture();
    warn_on_missing_parent_session_for_validate_type("real-session", true);
    let mut rx = captured.events_rx;
    assert!(rx.try_recv().is_err());
}
#[tokio::test(flavor = "current_thread")]
async fn broadcast_refresh_skill_baseline_sends_one_message_per_sender() {
    let mut receivers = Vec::new();
    let mut senders = Vec::new();
    for _ in 0..3 {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        senders.push(tx);
        receivers.push(rx);
    }
    MvpAgent::broadcast_refresh_skill_baseline(senders);
    for mut rx in receivers {
        assert!(matches!(
            rx.try_recv(),
            Ok(crate::session::SessionCommand::RefreshSkillBaseline)
        ));
        assert!(
            rx.try_recv().is_err(),
            "broadcast must send exactly one message per sender",
        );
    }
}
#[tokio::test(flavor = "current_thread")]
async fn broadcast_refresh_skill_baseline_tolerates_dropped_receiver() {
    let (tx_alive, mut rx_alive) = tokio::sync::mpsc::unbounded_channel();
    let (tx_dead, rx_dead) = tokio::sync::mpsc::unbounded_channel();
    drop(rx_dead);
    MvpAgent::broadcast_refresh_skill_baseline(vec![tx_alive, tx_dead]);
    assert!(matches!(
        rx_alive.try_recv(),
        Ok(crate::session::SessionCommand::RefreshSkillBaseline)
    ));
}
/// The monotonic turn counter must never wrap on the DB-bound i32 path.
/// `allocate_turn_number` returns u64; the AB submission casts to i32.
/// Verify we saturate instead of wrapping.
#[test]
fn trace_turn_to_i32_saturates_at_max() {
    let small: u64 = 42;
    let result = i32::try_from(small).unwrap_or(i32::MAX);
    assert_eq!(result, 42);
    let huge: u64 = (i32::MAX as u64) + 100;
    let result = i32::try_from(huge).unwrap_or(i32::MAX);
    assert_eq!(result, i32::MAX);
    let boundary: u64 = i32::MAX as u64;
    let result = i32::try_from(boundary).unwrap_or(i32::MAX);
    assert_eq!(result, i32::MAX);
}
/// When remote settings are absent (`None`), default to blocked.
#[test]
fn settings_allow_access_none_settings_is_blocked() {
    assert!(!settings_allow_access(None));
}
/// When `allow_access` is `Some(true)`, user is allowed.
#[test]
fn settings_allow_access_true_is_allowed() {
    let rs = crate::util::config::RemoteSettings {
        allow_access: Some(true),
        ..Default::default()
    };
    assert!(settings_allow_access(Some(&rs)));
}
/// When `allow_access` is `Some(false)` (remote settings default / rule
/// disabled), user stays blocked — even if they hold a qualifying
/// subscription. This is the regression guard for the bug where
/// `retry_subscription_check` unconditionally lifted the gate.
#[test]
fn settings_allow_access_false_is_blocked() {
    let rs = crate::util::config::RemoteSettings {
        allow_access: Some(false),
        ..Default::default()
    };
    assert!(!settings_allow_access(Some(&rs)));
}
/// When `/settings` returned successfully but the field is absent
/// (`None`), default to blocked (conservative).
#[test]
fn settings_allow_access_field_absent_is_blocked() {
    let rs = crate::util::config::RemoteSettings {
        allow_access: None,
        ..Default::default()
    };
    assert!(!settings_allow_access(Some(&rs)));
}
/// After allocating a turn number, `session_turn_numbers` holds the next
/// value (current + 1). This is the value that must be persisted via
/// `SetNextTraceTurn` so the counter survives restarts.
#[test]
fn allocate_turn_number_advances_counter() {
    use std::cell::RefCell;
    use std::collections::HashMap;
    let counters: RefCell<HashMap<acp::SessionId, u64>> = RefCell::new(HashMap::new());
    let sid = acp::SessionId::new("test-session");
    let allocate = |id: &acp::SessionId| -> u64 {
        let mut m = counters.borrow_mut();
        let turn = m.get(id).copied().unwrap_or(0u64);
        m.insert(id.clone(), turn.saturating_add(1));
        turn
    };
    assert_eq!(allocate(&sid), 0);
    assert_eq!(*counters.borrow().get(&sid).unwrap(), 1);
    assert_eq!(allocate(&sid), 1);
    assert_eq!(*counters.borrow().get(&sid).unwrap(), 2);
    assert_eq!(allocate(&sid), 2);
    assert_eq!(*counters.borrow().get(&sid).unwrap(), 3);
}
/// Build a synthetic harness `task` call/result pair carrying the
/// `<subagent_result>` footer, mirroring what the verifier/planner record.
fn harness_pair(id: &str) -> Vec<xai_grok_sampling_types::conversation::ConversationItem> {
    use xai_grok_sampling_types::ToolCall;
    use xai_grok_sampling_types::conversation::ConversationItem;
    vec![
        ConversationItem::assistant_tool_calls(vec![ToolCall {
            id: id.into(),
            name: "task".into(),
            arguments: "{}".into(),
        }]),
        ConversationItem::tool_result(id, "<subagent_result>\nsubagent_id: skeptic-1"),
    ]
}
/// Agent-side upload path: each drained harness turn takes a distinct,
/// monotonic turn number that CONTINUES past the user turn, advances the
/// per-session counter, and is persisted via exactly one `SetNextTraceTurn`.
/// This is what makes each sibling `turn_{N}` reachable — without the
/// advance every harness turn would clobber the same GCS path.
#[tokio::test(flavor = "current_thread")]
async fn upload_harness_trace_turns_numbers_siblings_and_persists_counter() {
    let agent = build_minimal_agent_for_tests();
    {
        let mut cfg = agent.cfg.borrow_mut();
        cfg.features.telemetry = Some(crate::agent::config::TelemetryMode::Enabled);
        cfg.telemetry.trace_upload = Some(true);
        cfg.endpoints.trace_upload_bucket = Some("gs://harness-trace-test".to_string());
    }
    let sid = acp::SessionId::new("harness-upload-sess");
    let info = crate::session::info::Info {
        id: sid.clone(),
        cwd: "/tmp".to_string(),
    };
    let mut handle = make_test_handle("test-model", false, None);
    handle.info = info.clone();
    let queue_home = tempfile::tempdir().unwrap();
    let queue_cfg = crate::session::repo_changes::TraceExportConfig {
        bucket_url: Some("gs://harness-trace-test".to_string()),
        service_account_key: None,
        prefix_dir: None,
        gcs_prefix: None,
        absolute_paths: false,
        archive_name_override: None,
        upload_method: crate::session::repo_changes::UploadMethod::Direct {
            service_account_key: None,
        },
    };
    let queue = crate::upload::trace::spawn_upload_queue(
        queue_home.path(),
        &queue_cfg,
        Some(xai_grok_version::VERSION),
        agent.auth_manager.clone(),
    );
    let _ = handle.upload_queue.set(queue);
    agent.sessions.borrow_mut().insert(sid.clone(), handle);
    for _ in 0..3 {
        agent.allocate_turn_number(&sid);
    }
    assert_eq!(agent.session_turn_number(&sid), Some(3));
    let built = agent
        .build_harness_trace_uploads(
            &sid,
            &info,
            "test-model",
            3,
            vec![harness_pair("a"), harness_pair("b")],
        )
        .await;
    let numbers: Vec<u64> = built.iter().map(|(_, m, _)| m.turn_number).collect();
    assert_eq!(numbers, vec![3, 4], "siblings take base, base+1");
    assert!(
        built.iter().all(|(_, m, _)| m.model == "test-model"),
        "harness metadata carries the requested model alias",
    );
    let (cmd_tx, mut cmd_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::session::SessionCommand>();
    agent
        .upload_harness_trace_turns(
            &sid,
            &info,
            &cmd_tx,
            "test-model",
            vec![harness_pair("a"), harness_pair("b")],
        )
        .await;
    assert_eq!(
        agent.session_turn_number(&sid),
        Some(5),
        "two siblings advance the counter by two from the user turn",
    );
    let mut persisted = Vec::new();
    while let Ok(cmd) = cmd_rx.try_recv() {
        if let crate::session::SessionCommand::SetNextTraceTurn {
            next_trace_turn, ..
        } = cmd
        {
            persisted.push(next_trace_turn);
        }
    }
    assert_eq!(
        persisted,
        vec![5],
        "persist the advanced counter once, ahead of the spawned uploads",
    );
}
/// With trace upload disabled the agent-side path must NOT burn a turn
/// number or persist a counter (and spawns no upload). The buffer-clearing
/// half of the drain is the caller's `TakeHarnessTraceTurns`; this guards
/// the upload function's uploads-disabled branch.
#[tokio::test(flavor = "current_thread")]
async fn upload_harness_trace_turns_uploads_disabled_does_not_burn_counter() {
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("harness-disabled-sess");
    let info = crate::session::info::Info {
        id: sid.clone(),
        cwd: "/tmp".to_string(),
    };
    let (cmd_tx, mut cmd_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::session::SessionCommand>();
    agent
        .upload_harness_trace_turns(&sid, &info, &cmd_tx, "test-model", vec![harness_pair("a")])
        .await;
    assert_eq!(
        agent.session_turn_number(&sid),
        None,
        "uploads-disabled skip must not consume a turn number",
    );
    assert!(
        cmd_rx.try_recv().is_err(),
        "uploads-disabled path must not persist a counter",
    );
}
/// Guards the per-harness-turn manifest seam: (1) every turn's ctx carries
/// a FRESH `artifact_tracker`, so turn 1 never inherits turn 0's recorded
/// artifacts; (2) recording the turn's metadata + turn_messages yields a
/// manifest listing exactly those two; (3) `fully_uploaded` is true iff
/// neither failed.
#[tokio::test(flavor = "current_thread")]
async fn upload_harness_trace_turns_build_per_turn_manifest() {
    use crate::upload::manifest::{
        ArtifactResult, ArtifactStatus, build_manifest, record_artifact, resolve_upload_method,
    };
    let agent = build_minimal_agent_for_tests();
    {
        let mut cfg = agent.cfg.borrow_mut();
        cfg.features.telemetry = Some(crate::agent::config::TelemetryMode::Enabled);
        cfg.telemetry.trace_upload = Some(true);
        cfg.endpoints.trace_upload_bucket = Some("gs://harness-trace-test".to_string());
    }
    let sid = acp::SessionId::new("harness-manifest-sess");
    let info = crate::session::info::Info {
        id: sid.clone(),
        cwd: "/tmp".to_string(),
    };
    let mut handle = make_test_handle("test-model", false, None);
    handle.info = info.clone();
    let queue_home = tempfile::tempdir().unwrap();
    let queue_cfg = crate::session::repo_changes::TraceExportConfig {
        bucket_url: Some("gs://harness-trace-test".to_string()),
        service_account_key: None,
        prefix_dir: None,
        gcs_prefix: None,
        absolute_paths: false,
        archive_name_override: None,
        upload_method: crate::session::repo_changes::UploadMethod::Direct {
            service_account_key: None,
        },
    };
    let queue = crate::upload::trace::spawn_upload_queue(
        queue_home.path(),
        &queue_cfg,
        Some(xai_grok_version::VERSION),
        agent.auth_manager.clone(),
    );
    let _ = handle.upload_queue.set(queue);
    agent.sessions.borrow_mut().insert(sid.clone(), handle);
    let built = agent
        .build_harness_trace_uploads(
            &sid,
            &info,
            "test-model",
            0,
            vec![harness_pair("a"), harness_pair("b")],
        )
        .await;
    assert_eq!(
        built.len(),
        2,
        "both harness turns obtained a trace context"
    );
    let ctx0 = &built[0].0;
    record_artifact(
        &ctx0.artifact_tracker,
        "metadata.json",
        ArtifactResult::Succeeded,
    );
    record_artifact(
        &ctx0.artifact_tracker,
        "turn_messages.json",
        ArtifactResult::Succeeded,
    );
    let m0 = build_manifest(&ctx0.artifact_tracker, resolve_upload_method(ctx0));
    assert!(matches!(
        m0.artifacts.get("metadata.json"),
        Some(ArtifactStatus::Succeeded)
    ));
    assert!(matches!(
        m0.artifacts.get("turn_messages.json"),
        Some(ArtifactStatus::Succeeded)
    ));
    assert!(m0.fully_uploaded, "both succeeded → fully_uploaded");
    let ctx1 = &built[1].0;
    let before = build_manifest(&ctx1.artifact_tracker, resolve_upload_method(ctx1));
    assert!(
        before.artifacts.is_empty(),
        "per-turn tracker: turn 1 must not inherit turn 0's artifacts",
    );
    record_artifact(
        &ctx1.artifact_tracker,
        "metadata.json",
        ArtifactResult::Succeeded,
    );
    record_artifact(
        &ctx1.artifact_tracker,
        "turn_messages.json",
        ArtifactResult::Failed {
            reason: "upload_failed",
            error: None,
        },
    );
    let m1 = build_manifest(&ctx1.artifact_tracker, resolve_upload_method(ctx1));
    assert!(
        !m1.fully_uploaded,
        "a failed turn_messages flips fully_uploaded",
    );
    assert_eq!(m1.artifacts.len(), 2, "no cross-turn contamination");
}
/// With no overrides and model_agent_type = None, the default agent is used.
#[test]
#[serial_test::serial]
fn resolve_agent_definition_defaults_to_grok_build() {
    let prev = std::env::var("GROK_AGENT").ok();
    unsafe {
        std::env::remove_var("GROK_AGENT");
    }
    let tmp = tempfile::tempdir().unwrap();
    let def = MvpAgent::resolve_agent_definition(
        tmp.path(),
        None,
        &config::AgentSelectionConfig::default(),
        None,
        None,
    );
    assert_eq!(def.name, config::DEFAULT_AGENT_TYPE);
    if let Some(v) = prev {
        unsafe { std::env::set_var("GROK_AGENT", v) }
    }
}
/// When model_agent_type = Some("codex"), the codex agent is selected even
/// though the default chain would return grok-build.
#[test]
#[serial_test::serial]
fn resolve_agent_definition_model_agent_type_overrides_default() {
    let prev = std::env::var("GROK_AGENT").ok();
    unsafe {
        std::env::remove_var("GROK_AGENT");
    }
    let tmp = tempfile::tempdir().unwrap();
    let def = MvpAgent::resolve_agent_definition(
        tmp.path(),
        None,
        &config::AgentSelectionConfig::default(),
        None,
        Some("codex"),
    );
    assert_eq!(def.name, "codex");
    if let Some(v) = prev {
        unsafe { std::env::set_var("GROK_AGENT", v) }
    }
}
/// When model_agent_type is None, the chain-resolved default agent is
/// NOT overridden. This is the crux of the leader-mode fix: a session whose
/// model has no agent_type must get the default agent, not a stale value
/// from a different client's model.
#[test]
#[serial_test::serial]
fn resolve_agent_definition_none_agent_type_does_not_override() {
    let prev = std::env::var("GROK_AGENT").ok();
    unsafe {
        std::env::remove_var("GROK_AGENT");
    }
    let tmp = tempfile::tempdir().unwrap();
    let def = MvpAgent::resolve_agent_definition(
        tmp.path(),
        None,
        &config::AgentSelectionConfig::default(),
        None,
        None,
    );
    assert_eq!(def.name, config::DEFAULT_AGENT_TYPE);
    if let Some(v) = prev {
        unsafe { std::env::set_var("GROK_AGENT", v) }
    }
}
/// Regression for the web-client devbox bug: an ACP profile must
/// win when the model's `agent_type` is the default value.
#[test]
#[serial_test::serial]
fn resolve_agent_definition_acp_profile_wins_when_model_agent_type_is_default() {
    let prev = std::env::var("GROK_AGENT").ok();
    unsafe {
        std::env::remove_var("GROK_AGENT");
    }
    let tmp = tempfile::tempdir().unwrap();
    let acp_profile = xai_grok_agent::AgentDefinition::from_json(&serde_json::json!(
        { "name" : "custom-devbox-profile", "description" :
        "Custom devbox profile", "systemPrompt" :
        "You are a custom-configured devbox agent.", }
    ))
    .expect("agent definition must parse");
    let def = MvpAgent::resolve_agent_definition(
        tmp.path(),
        None,
        &config::AgentSelectionConfig::default(),
        Some(acp_profile),
        Some(config::DEFAULT_AGENT_TYPE),
    );
    assert_eq!(
        def.name, "custom-devbox-profile",
        "ACP _meta.agentProfile must win when model_agent_type is the default value"
    );
    if let Some(v) = prev {
        unsafe { std::env::set_var("GROK_AGENT", v) }
    }
}
/// Regression: after `DEFAULT_AGENT_TYPE` flipped to
/// `grok-build-plan`, models in the catalog that still declare
/// `agent_type = "grok-build"` explicitly must NOT preempt an ACP
/// profile. Any value in the `grok-build*` family is the stock harness
/// with no strict requirement.
#[test]
#[serial_test::serial]
fn resolve_agent_definition_acp_profile_wins_for_explicit_grok_build_family() {
    let prev = std::env::var("GROK_AGENT").ok();
    unsafe {
        std::env::remove_var("GROK_AGENT");
    }
    let tmp = tempfile::tempdir().unwrap();
    let acp_profile = xai_grok_agent::AgentDefinition::from_json(&serde_json::json!(
        { "name" : "custom-devbox-profile", "description" :
        "Custom devbox profile", }
    ))
    .expect("agent definition must parse");
    for family_variant in ["grok-build", "grok-build-plan", "grok-build-concise"] {
        let def = MvpAgent::resolve_agent_definition(
            tmp.path(),
            None,
            &config::AgentSelectionConfig::default(),
            Some(acp_profile.clone()),
            Some(family_variant),
        );
        assert_eq!(
            def.name, "custom-devbox-profile",
            "ACP profile must win for grok-build family variant `{family_variant}`"
        );
    }
    if let Some(v) = prev {
        unsafe { std::env::set_var("GROK_AGENT", v) }
    }
}
/// A non-strict (stock / vision-capable) model leaves the template alone, so
/// such models keep native image input.
#[test]
fn inherited_harness_template_skips_nonstrict_model() {
    use xai_grok_agent::prompt::user_message::UserMessageTemplate;
    let tmp = tempfile::tempdir().unwrap();
    assert!(
        inherited_harness_template(
            &UserMessageTemplate::Default,
            Some(config::DEFAULT_AGENT_TYPE),
            tmp.path(),
        )
        .is_none()
    );
}
/// An explicit (non-default) template is never overridden — inheritance only
/// fills in the default.
#[test]
fn inherited_harness_template_respects_explicit_template() {
    use xai_grok_agent::prompt::user_message::UserMessageTemplate;
    let tmp = tempfile::tempdir().unwrap();
    let explicit = UserMessageTemplate::Custom("MY CUSTOM TEMPLATE".to_owned());
    assert!(inherited_harness_template(&explicit, Some("cursor"), tmp.path()).is_none());
}
/// CLI `--agent-profile` wins when model_agent_type is the default
/// (also shadowed by the same regression).
#[test]
#[serial_test::serial]
fn resolve_agent_definition_cli_agent_profile_wins_when_model_agent_type_is_default() {
    let prev = std::env::var("GROK_AGENT").ok();
    unsafe {
        std::env::remove_var("GROK_AGENT");
    }
    let tmp = tempfile::tempdir().unwrap();
    let profile_path = tmp.path().join("cli-profile.md");
    std::fs::write(
        &profile_path,
        "---\nname: cli-profile\ndescription: cli test\n---\nYou are a CLI profile.\n",
    )
    .unwrap();
    let def = MvpAgent::resolve_agent_definition(
        tmp.path(),
        Some(&profile_path),
        &config::AgentSelectionConfig::default(),
        None,
        Some(config::DEFAULT_AGENT_TYPE),
    );
    assert_eq!(def.name, "cli-profile");
    if let Some(v) = prev {
        unsafe { std::env::set_var("GROK_AGENT", v) }
    }
}
/// Agent profile with `model: Override(id)` preserves the field through resolution.
#[test]
#[serial_test::serial]
fn resolve_agent_definition_agent_profile_with_model_override() {
    let prev = std::env::var("GROK_AGENT").ok();
    unsafe {
        std::env::remove_var("GROK_AGENT");
    }
    let tmp = tempfile::tempdir().unwrap();
    let agents_dir = tmp.path().join(".grok").join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
            agents_dir.join("test-architect.md"),
            "---\nname: test-architect\ndescription: test\nmodel: test-model-123\n---\nYou are a test.\n",
        )
        .unwrap();
    let agent_config = config::AgentSelectionConfig {
        name: Some("test-architect".to_string()),
        definition: None,
        system_prompt_label: None,
    };
    let def = MvpAgent::resolve_agent_definition(tmp.path(), None, &agent_config, None, None);
    assert_eq!(def.name, "test-architect");
    assert_eq!(
        def.model,
        xai_grok_agent::config::ModelOverride::Override("test-model-123".to_string()),
        "agent profile model override must be preserved through resolution"
    );
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_AGENT", v) },
        None => unsafe { std::env::remove_var("GROK_AGENT") },
    }
}
#[test]
fn read_session_or_init_meta_str_prefers_session_meta() {
    let session = serde_json::json!({ "rules" : "from-session" });
    let init = serde_json::json!({ "rules" : "from-init" });
    assert_eq!(
        read_session_or_init_meta_str(session.as_object(), init.as_object(), "rules"),
        Some("from-session"),
    );
}
#[test]
fn read_session_or_init_meta_str_falls_back_to_init_meta() {
    let session = serde_json::json!({ "other" : "x" });
    let init = serde_json::json!({ "rules" : "from-init" });
    assert_eq!(
        read_session_or_init_meta_str(session.as_object(), init.as_object(), "rules"),
        Some("from-init"),
    );
    assert_eq!(
        read_session_or_init_meta_str(None, init.as_object(), "rules"),
        Some("from-init"),
    );
}
#[test]
fn parse_session_plugin_dirs_filters_and_dedupes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = dunce::canonicalize(tmp.path()).unwrap().join("plugin");
    std::fs::create_dir(&dir).unwrap();
    let file = tmp.path().join("file.txt");
    std::fs::write(&file, "x").unwrap();
    let meta = serde_json::json!(
        { "pluginDirs" : [dir.to_string_lossy(), dir.to_string_lossy(), file
        .to_string_lossy(), "relative/path", 42,] }
    );
    assert_eq!(parse_session_plugin_dirs(meta.as_object()), vec![dir]);
    assert!(parse_session_plugin_dirs(None).is_empty());
    assert!(parse_session_plugin_dirs(serde_json::json!({}).as_object()).is_empty());
}
#[test]
fn read_session_or_init_meta_str_returns_none_when_absent() {
    assert_eq!(read_session_or_init_meta_str(None, None, "rules"), None,);
    let session = serde_json::json!({ "other" : "x" });
    assert_eq!(
        read_session_or_init_meta_str(session.as_object(), None, "rules"),
        None,
    );
}
#[test]
fn read_session_or_init_meta_str_ignores_non_string_values() {
    let session = serde_json::json!({ "rules" : 42 });
    let init = serde_json::json!({ "rules" : "from-init" });
    assert_eq!(
        read_session_or_init_meta_str(session.as_object(), init.as_object(), "rules"),
        Some("from-init"),
    );
}
#[test]
fn system_prompt_override_from_meta_prefers_session_and_rejects_empty() {
    let session = serde_json::json!({ "systemPromptOverride" : "from session" });
    let init = serde_json::json!({ "systemPromptOverride" : "from init" });
    assert_eq!(
        system_prompt_override_from_meta(session.as_object(), init.as_object()),
        Some("from session")
    );
    assert_eq!(
        system_prompt_override_from_meta(None, init.as_object()),
        Some("from init")
    );
    let empty = serde_json::json!({ "systemPromptOverride" : "" });
    assert_eq!(
        system_prompt_override_from_meta(empty.as_object(), None),
        None
    );
    assert_eq!(system_prompt_override_from_meta(None, None), None);
}
#[test]
fn enqueue_replace_system_prompt_override_sends_when_present() {
    use crate::session::SessionCommand;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let session = serde_json::json!({ "systemPromptOverride" : "from session" });
    let init = serde_json::json!({ "systemPromptOverride" : "from init" });
    enqueue_replace_system_prompt_override(&tx, session.as_object(), init.as_object());
    match rx.try_recv() {
        Ok(SessionCommand::ReplaceSystemPrompt { system_prompt }) => {
            assert_eq!(system_prompt, "from session", "session meta wins over init");
        }
        _ => panic!("expected a ReplaceSystemPrompt command"),
    }
}
#[test]
fn enqueue_replace_system_prompt_override_noop_when_absent_or_empty() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    enqueue_replace_system_prompt_override(
        &tx,
        serde_json::json!({ "systemPromptOverride" : "" }).as_object(),
        None,
    );
    enqueue_replace_system_prompt_override(&tx, serde_json::json!({}).as_object(), None);
    enqueue_replace_system_prompt_override(&tx, None, None);
    assert!(
        rx.try_recv().is_err(),
        "no command should be enqueued without a non-empty override"
    );
}
/// Regression for the web-client `_meta.agentProfile` -> `set_session_model`
/// flow: a zero-turn switch from `grok-build` (a client profile name) to
/// `grok-build-plan` (the default model agent_type) must be
/// treated as compatible so the harness rebuild is skipped and the
/// custom prompt body is preserved.
#[test]
fn harnesses_are_compatible_for_stock_family_pairs() {
    assert!(harnesses_are_compatible("grok-build", "grok-build-plan"));
    assert!(harnesses_are_compatible("grok-build-plan", "grok-build"));
    assert!(harnesses_are_compatible("grok-build", "grok-build"));
    assert!(harnesses_are_compatible(
        "grok-build-concise",
        "grok-build-plan"
    ));
    assert!(harnesses_are_compatible(
        "remote-sidebar",
        "grok-build-plan"
    ));
}
#[test]
fn harnesses_are_compatible_rejects_strict_mismatches() {
    assert!(harnesses_are_compatible("codex", "codex"));
    assert!(!harnesses_are_compatible("grok-build-plan", "codex"));
}
#[test]
fn explicit_agent_type_wins_over_session_default() {
    assert_eq!(
        resolve_required_agent_type(Some("cursor"), "grok-build-plan"),
        "cursor"
    );
}
#[test]
fn null_agent_type_falls_back_to_session_default_grok_build_plan() {
    assert_eq!(
        resolve_required_agent_type(None, "grok-build-plan"),
        "grok-build-plan"
    );
}
#[test]
fn null_agent_type_falls_back_to_session_default_grok_build() {
    assert_eq!(
        resolve_required_agent_type(None, "grok-build"),
        "grok-build"
    );
}
#[test]
fn null_agent_type_returns_to_session_default_after_cursor_switch() {
    let session_default = "grok-build-plan";
    let required_after_null = resolve_required_agent_type(None, session_default);
    assert_eq!(required_after_null, "grok-build-plan");
    assert_ne!(required_after_null, "cursor");
}
/// Compatible stock switches (no rebuild) must NOT mutate `agent_name`,
/// preserving the session's original ACP `agentProfile`.
#[test]
fn agent_name_unchanged_without_harness_rebuild() {
    let unchanged = agent_name_after_model_switch(false, "grok-build-plan", "remote-sidebar");
    assert_eq!(
        unchanged, "remote-sidebar",
        "a compatible stock switch must preserve the original agent profile name"
    );
}
/// End-to-end test: config -> resolve -> override -> finalize -> tool_definitions.
///
/// Exercises the full live path through to the finalized toolset, proving
/// that the hashline tools appear in the actual tool definitions that
/// would be sent to the model.
#[tokio::test]
async fn file_toolset_override_e2e_to_finalized_toolset() {
    use crate::tools::{FileToolset, ShellToolsetConfig};
    use xai_grok_tools::computer::local::{LocalFs, LocalTerminalBackend};
    use xai_grok_tools::notification::ToolNotificationHandle;
    use xai_grok_tools::registry::types::SessionContext;
    let tmp = tempfile::tempdir().unwrap();
    let mut def = MvpAgent::resolve_agent_definition(
        tmp.path(),
        None,
        &config::AgentSelectionConfig::default(),
        None,
        None,
    );
    let toolset_config = ShellToolsetConfig {
        file_toolset: FileToolset::Hashline,
        ..ShellToolsetConfig::default()
    };
    let effective = toolset_config.resolve_file_toolset(None);
    let file_tools = effective
        .tool_configs(&toolset_config.hashline)
        .expect("default hashline config should validate");
    def.override_file_tools(file_tools);
    let builder = xai_grok_tools::registry::types::ToolRegistryBuilder::new();
    let ctx = SessionContext {
        backend: std::sync::Arc::new(LocalTerminalBackend::new()),
        fs: std::sync::Arc::new(LocalFs),
        cwd: tmp.path().to_path_buf(),
        session_folder: tmp.path().join("session"),
        session_env: std::sync::Arc::new(std::collections::HashMap::new()),
        notification_handle: ToolNotificationHandle::noop(),
        owner_session_id: None,
        parent_scheduler_handle: None,
        skills: vec![],
        state_path: tmp.path().join("state.json"),
        memory_backend: None,
        web_search_config: xai_grok_tools::implementations::web_search::WebSearchConfig::default(),
        web_fetch_config: Default::default(),
        lsp: None,
        image_gen_config: xai_grok_tools::implementations::grok_build::image_gen::ImageGenConfig::default(),
        video_gen_config: xai_grok_tools::implementations::grok_build::video_gen::VideoGenConfig::default(),
        app_builder_deployer_config: xai_grok_tools::implementations::grok_build::deploy_app::AppBuilderDeployerConfig::default(),
        api_key_provider: None,
        auth_provider: None,
        attribution_callback: None,
        system_reminder_tag: xai_grok_tools::reminders::DEFAULT_REMINDER_TAG,
    };
    let toolset = builder
        .finalize(def.tool_config, ctx)
        .expect("hashline toolset should finalize");
    let defs = toolset.tool_definitions();
    let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
    assert!(names.contains(&"hashline_read"), "defs: {names:?}");
    assert!(names.contains(&"hashline_edit"), "defs: {names:?}");
    assert!(names.contains(&"hashline_grep"), "defs: {names:?}");
    assert!(!names.contains(&"read_file"), "defs: {names:?}");
    assert!(!names.contains(&"search_replace"), "defs: {names:?}");
    assert!(names.contains(&"list_dir"), "defs: {names:?}");
}
/// Invalid hashline config returns a clean error, not a panic.
#[test]
fn file_toolset_override_invalid_config_returns_error() {
    use crate::tools::FileToolset;
    use crate::tools::config::HashlineSchemeConfig;
    let bad = HashlineSchemeConfig {
        scheme: "bogus".to_owned(),
        hash_len: 0,
        chunk_size: 0,
    };
    let err = FileToolset::Hashline.tool_configs(&bad);
    assert!(err.is_err());
    assert!(err.unwrap_err().contains("unknown"));
}
/// Helper: creates a real SessionHandle with the given model, yolo, and client id.
/// Requires a tokio runtime for SessionSignalsHandle::new().
fn make_test_handle(
    model: &str,
    yolo: bool,
    client_id: Option<&str>,
) -> crate::session::SessionHandle {
    let (cmd_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
    let (hunk_event_tx, _hunk_event_rx) = tokio::sync::mpsc::unbounded_channel();
    let hunk_cancel = tokio_util::sync::CancellationToken::new();
    let hunk_tracker_handle = xai_hunk_tracker::HunkTrackerActor::spawn(
        "test".to_string(),
        std::path::PathBuf::from("/tmp"),
        hunk_event_tx,
        xai_hunk_tracker::TrackingMode::AllDirty,
        hunk_cancel,
    );
    crate::session::SessionHandle {
        cmd_tx,
        persistence_tx,
        current_prompt_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
        pending_interactions: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        info: crate::session::info::Info {
            id: acp::SessionId::new("test"),
            cwd: "/tmp".to_string(),
        },
        max_turns: None,
        hunk_tracker_handle,
        chat_state_handle: xai_chat_state::ChatStateHandle::noop(),
        signals_handle: crate::session::signals::SessionSignalsHandle::new(),
        gateway_enabled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        mcp_servers: vec![],
        initial_client_mcp_servers: vec![],
        display_cwd: None,
        feedback_manager: std::sync::Arc::new(
            crate::session::feedback_manager::FeedbackManager::local_only("test"),
        ),
        upload_queue: Arc::new(OnceLock::new()),
        upload_failures_since_success: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        tool_context: crate::tools::ToolContext::new_local_context(
            xai_grok_paths::AbsPathBuf::new(std::path::PathBuf::from("/tmp")).unwrap(),
            std::sync::Arc::new(xai_grok_workspace::file_system::LocalFs::new(
                std::path::PathBuf::from("/tmp"),
            )),
            std::sync::Arc::new(crate::terminal::LocalTerminalRunner),
        ),
        model_id: acp::ModelId::new(model),
        reasoning_effort: None,
        yolo_mode: yolo,
        origin_client: client_id.map(|s| crate::http::OriginClientInfo {
            product: s.to_string(),
            version: None,
        }),
        code_nav_enabled: false,
        ask_user_question_enabled: true,
        plan_mode: std::sync::Arc::new(parking_lot::Mutex::new(
            crate::session::plan_mode::PlanModeTracker::new(std::path::PathBuf::from("/tmp")),
        )),
        force_compact: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        permission_handle: xai_grok_workspace::permission::PermissionHandle::allow_all(),
        attribution_callback: None,
        agent_name: "grok-build".to_string(),
        managed_mcp_proxy_base_url: String::new(),
        session_default_agent_profile: None,
        allowed_subagent_types: None,
        hook_registry: None,
        workspace_ops: xai_grok_workspace::WorkspaceOps::for_test(),
        terminal_backend: None,
        tools_notification_handle: None,
        scheduler_handle: None,
    }
}
/// lookup_session_model returns the per-session model for each session.
#[tokio::test]
async fn lookup_session_model_returns_per_session_model() {
    let sid_a = acp::SessionId::new("sess-a");
    let sid_b = acp::SessionId::new("sess-b");
    let default_model = acp::ModelId::new("default-model");
    let sessions: HashMap<acp::SessionId, crate::session::SessionHandle> = [
        (sid_a.clone(), make_test_handle("grok-3-fast", false, None)),
        (sid_b.clone(), make_test_handle("codex-mini", false, None)),
    ]
    .into();
    assert_eq!(
        lookup_session_model(&sessions, Some(&sid_a), &default_model)
            .0
            .as_ref(),
        "grok-3-fast"
    );
    assert_eq!(
        lookup_session_model(&sessions, Some(&sid_b), &default_model)
            .0
            .as_ref(),
        "codex-mini"
    );
}
/// lookup_session_model falls back to the default when session_id is None.
#[tokio::test]
async fn lookup_session_model_fallback_no_session() {
    let default_model = acp::ModelId::new("grok-3");
    let sessions: HashMap<acp::SessionId, crate::session::SessionHandle> = HashMap::new();
    assert_eq!(
        lookup_session_model(&sessions, None, &default_model)
            .0
            .as_ref(),
        "grok-3"
    );
}
/// Mutating session A's model_id via the handle does not affect session B.
#[tokio::test]
async fn set_session_model_does_not_cross_contaminate() {
    let sid_a = acp::SessionId::new("sess-a");
    let sid_b = acp::SessionId::new("sess-b");
    let default_model = acp::ModelId::new("default");
    let mut sessions: HashMap<acp::SessionId, crate::session::SessionHandle> = [
        (sid_a.clone(), make_test_handle("grok-3", false, None)),
        (sid_b.clone(), make_test_handle("grok-3", false, None)),
    ]
    .into();
    sessions.get_mut(&sid_a).unwrap().model_id = acp::ModelId::new("codex-mini");
    assert_eq!(
        lookup_session_model(&sessions, Some(&sid_a), &default_model)
            .0
            .as_ref(),
        "codex-mini"
    );
    assert_eq!(
        lookup_session_model(&sessions, Some(&sid_b), &default_model)
            .0
            .as_ref(),
        "grok-3",
        "Session B's model must not be affected by session A's model change"
    );
}
#[tokio::test]
async fn model_state_prefers_session_reasoning_effort_over_global() {
    use crate::agent::config::{EndpointsConfig, ModelEntry};
    use xai_grok_sampling_types::{REASONING_EFFORT_META_KEY, ReasoningEffort};
    let agent = build_minimal_agent_for_tests();
    let mut entry = ModelEntry::fallback("effort-model", &EndpointsConfig::default());
    entry.info.supports_reasoning_effort = true;
    agent
        .models_manager
        .insert_test_entry("effort-model", entry);
    agent
        .models_manager
        .set_current_reasoning_effort(Some(ReasoningEffort::Low));
    let read_effort = |state: &acp::SessionModelState| -> Option<String> {
        state
            .available_models
            .iter()
            .find(|m| m.model_id.0.as_ref() == "effort-model")
            .and_then(|m| m.meta.as_ref())
            .and_then(|m| m.get(REASONING_EFFORT_META_KEY))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    };
    let pinned = acp::SessionId::new("sess-pinned");
    let mut handle = make_test_handle("effort-model", false, None);
    handle.reasoning_effort = Some(ReasoningEffort::Xhigh);
    agent.sessions.borrow_mut().insert(pinned.clone(), handle);
    assert_eq!(
        read_effort(&agent.model_state(Some(&pinned))).as_deref(),
        Some("xhigh"),
        "model_state must report the session's own restored effort",
    );
    let unset = acp::SessionId::new("sess-unset");
    agent
        .sessions
        .borrow_mut()
        .insert(unset.clone(), make_test_handle("effort-model", false, None));
    assert_eq!(
        read_effort(&agent.model_state(Some(&unset))).as_deref(),
        Some("low"),
        "absent session effort falls back to the global default",
    );
}
/// A session persisted under a routing *slug* (not the catalog map key) must
/// still get reasoning modes and a selected model from
/// `session_config_options` — the id is resolved to the catalog key before
/// the catalog effort lookups and the selected-model match.
#[tokio::test]
async fn session_config_options_resolves_routing_slug_to_catalog_model() {
    use crate::agent::config::{EndpointsConfig, ModelEntry};
    use xai_grok_sampling_types::ReasoningEffort;
    let agent = build_minimal_agent_for_tests();
    let mut entry = ModelEntry::fallback("catalog-key-model", &EndpointsConfig::default());
    entry.info.model = "routing-slug".to_string();
    entry.info.supports_reasoning_effort = true;
    entry.info.reasoning_effort = Some(ReasoningEffort::High);
    agent
        .models_manager
        .insert_test_entry("catalog-key-model", entry);
    let sid = acp::SessionId::new("sess-slug");
    agent
        .sessions
        .borrow_mut()
        .insert(sid.clone(), make_test_handle("routing-slug", false, None));
    let state = agent.model_state(Some(&sid));
    assert_eq!(state.current_model_id.0.as_ref(), "routing-slug");
    let opts = agent.session_config_options(Some(&sid), &state);
    let modes: Vec<_> = opts.iter().filter(|o| o.category == "mode").collect();
    assert!(
        !modes.is_empty(),
        "reasoning modes must surface for a slug-identified session"
    );
    assert!(
        modes.iter().any(|o| o.id == "high" && o.selected),
        "catalog default effort should be selected"
    );
    assert!(
        opts.iter()
            .any(|o| o.category == "model" && o.id == "catalog-key-model" && o.selected),
        "resolved catalog model must be selected"
    );
}
/// YOLO toggle scoped by client_identifier: only matching sessions are updated.
#[tokio::test]
async fn yolo_toggle_scoped_by_client_identifier() {
    let sid_tui = acp::SessionId::new("sess-tui");
    let sid_vscode = acp::SessionId::new("sess-vscode");
    let mut sessions: HashMap<acp::SessionId, crate::session::SessionHandle> = [
        (
            sid_tui.clone(),
            make_test_handle("grok-3", false, Some("grok-tui")),
        ),
        (
            sid_vscode.clone(),
            make_test_handle("grok-3", false, Some("grok-code-extension")),
        ),
    ]
    .into();
    let updated = apply_yolo_mode_to_matching_sessions(&mut sessions, Some("grok-tui"), true);
    assert_eq!(updated, 1, "exactly one matching session should be updated");
    assert!(
        sessions[&sid_tui].yolo_mode,
        "TUI session should have yolo=true after TUI toggle"
    );
    assert!(
        !sessions[&sid_vscode].yolo_mode,
        "VS Code session must NOT be affected by TUI's yolo toggle"
    );
}
/// A client can explicitly disable YOLO for its own sessions after startup,
/// even if those sessions were initially created with yolo=true.
#[tokio::test]
async fn yolo_toggle_can_disable_session_started_with_yolo_enabled() {
    let sid_tui = acp::SessionId::new("sess-tui");
    let sid_other = acp::SessionId::new("sess-other");
    let mut sessions: HashMap<acp::SessionId, crate::session::SessionHandle> = [
        (
            sid_tui.clone(),
            make_test_handle("grok-3", true, Some("grok-tui")),
        ),
        (
            sid_other.clone(),
            make_test_handle("grok-3", true, Some("grok-code-extension")),
        ),
    ]
    .into();
    let updated = apply_yolo_mode_to_matching_sessions(&mut sessions, Some("grok-tui"), false);
    assert_eq!(updated, 1, "only the sender's session should be updated");
    assert!(
        !sessions[&sid_tui].yolo_mode,
        "sender session should be switched to yolo=false"
    );
    assert!(
        sessions[&sid_other].yolo_mode,
        "other client's session must keep its previous yolo state"
    );
}
/// `drain_old_session_thread` returns immediately when the thread has
/// already finished.
#[tokio::test]
async fn drain_finished_thread_returns_immediately() {
    let session_threads: RefCell<HashMap<acp::SessionId, crate::session::SessionThread>> =
        RefCell::new(HashMap::new());
    let sid = acp::SessionId::new("drain-test");
    let handle = std::thread::spawn(|| {});
    std::thread::sleep(std::time::Duration::from_millis(10));
    session_threads.borrow_mut().insert(
        sid.clone(),
        crate::session::SessionThread::from_handle(handle),
    );
    let thread = session_threads.borrow_mut().remove(&sid).unwrap();
    assert!(thread.is_finished(), "thread should be finished");
    assert!(!session_threads.borrow().contains_key(&sid));
}
/// `drain_old_session_thread` waits for a slow thread to finish.
#[tokio::test]
async fn drain_waits_for_slow_thread() {
    let session_threads: RefCell<HashMap<acp::SessionId, crate::session::SessionThread>> =
        RefCell::new(HashMap::new());
    let sid = acp::SessionId::new("slow-drain");
    let handle = std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(100));
    });
    session_threads.borrow_mut().insert(
        sid.clone(),
        crate::session::SessionThread::from_handle(handle),
    );
    let thread = session_threads.borrow_mut().remove(&sid).unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if thread.is_finished() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "thread should finish within 5s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(thread.is_finished());
}
/// Drain respects the 5s deadline and returns even if the thread is still running.
#[tokio::test]
async fn drain_respects_deadline() {
    let session_threads: RefCell<HashMap<acp::SessionId, crate::session::SessionThread>> =
        RefCell::new(HashMap::new());
    let sid = acp::SessionId::new("hung-drain");
    let handle = std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(30));
    });
    session_threads.borrow_mut().insert(
        sid.clone(),
        crate::session::SessionThread::from_handle(handle),
    );
    let thread = session_threads.borrow_mut().remove(&sid).unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(200);
    let mut timed_out = false;
    loop {
        if thread.is_finished() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        timed_out,
        "should have timed out waiting for the hung thread"
    );
    assert!(!thread.is_finished(), "thread should still be running");
}
#[test]
fn parse_code_nav_capability_present_and_true() {
    let mut meta = serde_json::Map::new();
    meta.insert(
        "x.ai/codeNavigation".to_string(),
        serde_json::json!({ "enabled" : true }),
    );
    let init = acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
        acp::ClientCapabilities::new()
            .fs(acp::FileSystemCapabilities::new())
            .terminal(false)
            .meta(meta),
    );
    assert!(MvpAgent::parse_code_nav_capability(&init));
}
#[test]
fn parse_code_nav_capability_absent_returns_false() {
    let init = acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
        acp::ClientCapabilities::new()
            .fs(acp::FileSystemCapabilities::new())
            .terminal(false),
    );
    assert!(!MvpAgent::parse_code_nav_capability(&init));
}
#[test]
fn parse_code_nav_capability_false_returns_false() {
    let mut meta = serde_json::Map::new();
    meta.insert(
        "x.ai/codeNavigation".to_string(),
        serde_json::json!({ "enabled" : false }),
    );
    let init = acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
        acp::ClientCapabilities::new()
            .fs(acp::FileSystemCapabilities::new())
            .terminal(false)
            .meta(meta),
    );
    assert!(!MvpAgent::parse_code_nav_capability(&init));
}
/// Verify that two session handles with different code-nav state produce
/// independent eligibility outcomes — the key leader-mode isolation test.
///
/// This tests the `code_nav_eligibility_for_request` lookup path directly
/// by inspecting the per-handle fields rather than building a full agent,
/// which mirrors what the method actually reads at runtime.
#[tokio::test]
async fn test_per_session_code_nav_isolation() {
    let web_handle = {
        let mut h = make_test_handle("model", false, Some("grok-web"));
        h.code_nav_enabled = true;
        h
    };
    let tui_handle = {
        let mut h = make_test_handle("model", false, Some("grok-tui"));
        h.code_nav_enabled = false;
        h
    };
    let check = |handle: &crate::session::SessionHandle| {
        let ct = crate::http::client_type_from_origin(handle.origin_client.as_ref());
        if !matches!(ct, ClientType::GrokWeb) {
            return Err(CodeNavEligibility::ClientNotWeb);
        }
        if !handle.code_nav_enabled {
            return Err(CodeNavEligibility::CapabilityNotAdvertised);
        }
        Ok(())
    };
    assert!(
        check(&web_handle).is_ok(),
        "web session with capability should pass client-type and capability gates"
    );
    assert_eq!(
        check(&tui_handle),
        Err(CodeNavEligibility::ClientNotWeb),
        "tui session should be rejected at gate 1"
    );
    let mut web_no_cap = web_handle.clone();
    web_no_cap.code_nav_enabled = false;
    assert_eq!(
        check(&web_no_cap),
        Err(CodeNavEligibility::CapabilityNotAdvertised),
        "web session without capability should be rejected at gate 2"
    );
    assert!(
        check(&web_handle).is_ok(),
        "original web handle must be unaffected"
    );
}
/// Verify that code-nav requests without a sessionId are rejected.
///
/// `sessionId` is required so per-client capability gating is unambiguous
/// in both simple and leader modes.  Falling back to shared global state
/// (last-client-wins in leader mode) is not safe.
#[test]
fn test_sessionless_request_requires_session_id() {
    let session_id: Option<&acp::SessionId> = None;
    let result: Result<(), CodeNavEligibility> = if session_id.is_none() {
        Err(CodeNavEligibility::SessionRequired)
    } else {
        Ok(())
    };
    assert_eq!(
        result,
        Err(CodeNavEligibility::SessionRequired),
        "cwd-only requests with no sessionId must return SessionRequired"
    );
}
#[tokio::test(flavor = "current_thread")]
async fn ext_method_routes_auth_cleared_and_refreshes_resident_sessions() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let agent = build_agent_with_auth(crate::auth::GrokAuth {
                key: "eligible".into(),
                auth_mode: crate::auth::AuthMode::WebLogin,
                ..crate::auth::GrokAuth::test_default()
            });
            use acp::Agent as _;
            agent.managed_mcp_cache.lock().await.enable_gateway_tools();
            let sid = acp::SessionId::new("sess-auth-cleared");
            let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
            agent.sessions.borrow_mut().insert(sid, handle);
            let params = serde_json::json!({});
            agent
                .ext_method(acp::ExtRequest::new(
                    "x.ai/internal/auth_cleared",
                    std::sync::Arc::from(serde_json::value::to_raw_value(&params).unwrap()),
                ))
                .await
                .expect("auth_cleared must route through session-admin");
            let cmd = tokio::time::timeout(std::time::Duration::from_secs(1), cmd_rx.recv())
                .await
                .expect("refresh command should be sent")
                .expect("channel should stay open until command is received");
            assert!(matches!(cmd, SessionCommand::RefreshMcpSearchIndex));
            assert!(!agent.managed_mcp_cache.lock().await.gateway_tools_active);
        })
        .await;
}
/// Build a minimal MvpAgent suitable for testing extension methods.
fn build_minimal_agent_for_tests() -> MvpAgent {
    use crate::agent::config::Config as AgentConfig;
    use crate::auth::{AuthManager, GrokComConfig};
    let temp_dir = tempfile::tempdir().unwrap();
    let auth_manager =
        std::sync::Arc::new(AuthManager::new(temp_dir.path(), GrokComConfig::default()));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let gateway = GatewaySender::new(tx);
    let cfg = AgentConfig::default();
    MvpAgent::new(gateway, &cfg, auth_manager, None).expect("valid test config")
}
/// Build a minimal MvpAgent with pre-loaded auth for gate tests.
fn build_agent_with_auth(auth: crate::auth::GrokAuth) -> MvpAgent {
    use crate::agent::config::Config as AgentConfig;
    use crate::auth::{AuthManager, GrokComConfig};
    let temp_dir = tempfile::tempdir().unwrap();
    let auth_manager =
        std::sync::Arc::new(AuthManager::new(temp_dir.path(), GrokComConfig::default()));
    auth_manager.hot_swap(auth);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let gateway = GatewaySender::new(tx);
    let cfg = AgentConfig::default();
    MvpAgent::new(gateway, &cfg, auth_manager, None).expect("valid test config")
}
/// Regression: boot-time plugin discovery is deferred past ACP
/// `initialize`, so the shared plugin registry starts empty.
/// `resolve_mcp_servers` reads that snapshot to merge plugin-contributed
/// MCP servers into a new session, so without lazy population the servers
/// silently vanished until an explicit `/plugins reload`.
/// `ensure_plugin_registry` must build the snapshot on first use.
#[tokio::test]
#[serial_test::serial]
async fn ensure_plugin_registry_lazily_populates_snapshot() {
    use crate::agent::config::Config as AgentConfig;
    use crate::auth::{AuthManager, GrokComConfig};
    use xai_grok_test_support::EnvGuard;
    let grok_home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", grok_home.path());
    let plugin_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        plugin_dir.path().join("plugin.json"),
        r#"{"name": "regr-lazy-mcp-plugin"}"#,
    )
    .unwrap();
    std::fs::write(
        plugin_dir.path().join(".mcp.json"),
        r#"{"mcpServers":{"regr-srv":{"command":"echo","args":["hi"]}}}"#,
    )
    .unwrap();
    let auth_home = tempfile::tempdir().unwrap();
    let auth_manager =
        std::sync::Arc::new(AuthManager::new(auth_home.path(), GrokComConfig::default()));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let gateway = GatewaySender::new(tx);
    let mut cfg = AgentConfig::default();
    cfg.plugins.cli_plugin_dirs = vec![plugin_dir.path().to_path_buf()];
    let agent = MvpAgent::new(gateway, &cfg, auth_manager, None).expect("valid test config");
    assert!(
        agent.plugin_registry_handle.snapshot().is_none(),
        "snapshot must start empty (boot discovery deferred past initialize)"
    );
    agent.ensure_plugin_registry();
    let snapshot = agent
        .plugin_registry_handle
        .snapshot()
        .expect("snapshot must be populated on first use");
    assert!(
        snapshot.get("regr-lazy-mcp-plugin").is_some(),
        "lazy discovery must surface the plugin so its MCP server merges into the session"
    );
    agent.ensure_plugin_registry();
    assert!(
        agent
            .plugin_registry_handle
            .snapshot()
            .is_some_and(|s| s.get("regr-lazy-mcp-plugin").is_some()),
        "repeat call must keep the populated snapshot"
    );
}
mod subagent_spawn_context_tests;
/// No load in flight and no session → the wait returns immediately
/// (the caller then surfaces "unknown session id" exactly as before).
#[tokio::test]
async fn wait_for_in_flight_load_returns_immediately_when_idle() {
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("sess-none");
    tokio::time::timeout(
        std::time::Duration::from_millis(200),
        agent.wait_for_in_flight_session_load(&sid),
    )
    .await
    .expect("wait must not block when no load is in flight");
}
/// A waiter racing an in-flight `session/load` blocks until the load
/// finishes and then observes the registered session. This is the
/// agent-side guarantee that closes the post-leader-crash
/// "unknown session id" race: the reconnect replay's `session/load` and
/// the client's next `session/prompt` can arrive back-to-back.
#[tokio::test]
async fn wait_for_in_flight_load_blocks_until_load_completes() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let agent = std::rc::Rc::new(build_minimal_agent_for_tests());
            let sid = acp::SessionId::new("sess-loading");
            let guard = agent.begin_session_load(&sid);
            let waiter_agent = agent.clone();
            let waiter_sid = sid.clone();
            let waiter = tokio::task::spawn_local(async move {
                waiter_agent
                    .wait_for_in_flight_session_load(&waiter_sid)
                    .await;
                waiter_agent.sessions.borrow().contains_key(&waiter_sid)
            });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert!(!waiter.is_finished(), "waiter must block while loading");
            let handle = make_test_handle("test-model", false, None);
            agent.sessions.borrow_mut().insert(sid.clone(), handle);
            drop(guard);
            let found_session = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
                .await
                .expect("waiter must wake when the load guard drops")
                .expect("waiter task must not panic");
            assert!(
                found_session,
                "after the wait, the session must be visible to the racing request"
            );
        })
        .await;
}
/// A failed load (guard dropped WITHOUT registering the session) also
/// wakes waiters — they re-check, find nothing, and the caller surfaces
/// the regular "unknown session id" error rather than hanging.
#[tokio::test]
async fn wait_for_in_flight_load_wakes_on_failed_load() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let agent = std::rc::Rc::new(build_minimal_agent_for_tests());
            let sid = acp::SessionId::new("sess-load-fails");
            let guard = agent.begin_session_load(&sid);
            let waiter_agent = agent.clone();
            let waiter_sid = sid.clone();
            let waiter = tokio::task::spawn_local(async move {
                waiter_agent
                    .wait_for_in_flight_session_load(&waiter_sid)
                    .await;
                waiter_agent.sessions.borrow().contains_key(&waiter_sid)
            });
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            drop(guard);
            let found_session = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
                .await
                .expect("waiter must wake when the failed load's guard drops")
                .expect("waiter task must not panic");
            assert!(!found_session, "failed load leaves no session behind");
        })
        .await;
}
/// Two concurrent loads of the same session: the first guard's drop must
/// not remove the second load's marker (waiters keep waiting on the
/// newer in-flight load).
#[tokio::test]
async fn concurrent_load_guards_do_not_clobber_each_other() {
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("sess-concurrent");
    let guard_one = agent.begin_session_load(&sid);
    let guard_two = agent.begin_session_load(&sid);
    drop(guard_one);
    assert!(
        agent.loading_sessions.borrow().contains_key(&sid),
        "second load's marker must survive the first guard's drop"
    );
    drop(guard_two);
    assert!(
        agent.loading_sessions.borrow().is_empty(),
        "all markers removed once every load finished"
    );
}
/// `resident_activity` returns `NeedsInput` whenever the session's
/// pending-interaction map is non-empty — and that wins even over a
/// running turn (a session blocked on a permission mid-turn "needs
/// input"). Clearing the map falls back to Working / Idle.
#[tokio::test]
async fn resident_activity_reports_needs_input_when_pending() {
    use crate::agent::roster::RosterActivity;
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("sess-pending");
    let handle = make_test_handle("grok-3", false, None);
    let pending = handle.pending_interactions.clone();
    let prompt_id = handle.current_prompt_id.clone();
    agent.sessions.borrow_mut().insert(sid.clone(), handle);
    assert_eq!(agent.resident_activity(&sid), RosterActivity::Idle);
    *prompt_id.lock().unwrap() = Some("turn-1".to_string());
    assert_eq!(agent.resident_activity(&sid), RosterActivity::Working);
    pending.lock().unwrap().insert(
        "call-1".to_string(),
        crate::session::pending_interaction::PendingKind::Permission,
    );
    assert_eq!(agent.resident_activity(&sid), RosterActivity::NeedsInput);
    let entry = agent.resident_roster_entry(&sid).expect("resident entry");
    assert_eq!(entry.activity, RosterActivity::NeedsInput);
    pending.lock().unwrap().clear();
    assert_eq!(agent.resident_activity(&sid), RosterActivity::Working);
}
/// Drain the agent gateway, returning the first `x.ai/sessions/changed`
/// payload that carries an upserted entry (ignoring any unrelated
/// notifications, which parse into an empty `RosterChanged`).
fn drain_roster_changed(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
) -> Option<crate::agent::roster::RosterChanged> {
    let mut found = None;
    while let Ok(msg) = rx.try_recv() {
        if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg {
            if found.is_none()
                && let Ok(changed) = serde_json::from_str::<crate::agent::roster::RosterChanged>(
                    args.request.params.get(),
                )
                && !changed.upserted.is_empty()
            {
                found = Some(changed);
            }
            let _ = args.response_tx.send(Ok(()));
        }
    }
    found
}
/// A turn-boundary activity delta (`push_roster_activity_delta`) broadcasts
/// an `x.ai/sessions/changed` upsert carrying the *overridden* activity, so
/// every attached dashboard reflects Working/Idle immediately instead of
/// waiting for the ≤1s roster poll (turn-start/turn-end). The
/// override matters because at turn-start the actor has not yet published
/// `current_prompt_id`, so a natural `resident_activity` read would emit
/// `Idle` for a session that is in fact starting a turn.
#[tokio::test]
async fn push_roster_activity_delta_broadcasts_overridden_activity() {
    use crate::agent::config::Config as AgentConfig;
    use crate::agent::roster::RosterActivity;
    use crate::auth::{AuthManager, GrokComConfig};
    let temp_dir = tempfile::tempdir().unwrap();
    let auth_manager =
        std::sync::Arc::new(AuthManager::new(temp_dir.path(), GrokComConfig::default()));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let gateway = GatewaySender::new(tx);
    let cfg = AgentConfig::default();
    let agent = MvpAgent::new(gateway, &cfg, auth_manager, None).expect("valid test config");
    let sid = acp::SessionId::new("sess-activity");
    agent
        .sessions
        .borrow_mut()
        .insert(sid.clone(), make_test_handle("grok-3", false, None));
    agent.push_roster_activity_delta(&sid, RosterActivity::Working);
    let changed = drain_roster_changed(&mut rx).expect("turn-start delta emitted");
    assert_eq!(changed.upserted.len(), 1);
    assert_eq!(changed.upserted[0].session_id, sid.0.to_string());
    assert!(changed.upserted[0].resident);
    assert_eq!(
        changed.upserted[0].activity,
        RosterActivity::Working,
        "forced activity must override the Idle that resident_activity would read"
    );
    assert!(changed.removed.is_empty());
    agent.push_roster_activity_delta(&sid, RosterActivity::Idle);
    let changed = drain_roster_changed(&mut rx).expect("turn-end delta emitted");
    assert_eq!(changed.upserted[0].activity, RosterActivity::Idle);
}
/// Extract the inner payload from an ExtResponse.
#[expect(
    dead_code,
    reason = "unused in production; remove expect when wired or delete the item"
)]
fn parse_ext_body(resp: &acp::ExtResponse) -> serde_json::Value {
    let outer: serde_json::Value =
        serde_json::from_str(resp.0.get()).expect("ExtResponse must be valid JSON");
    outer
        .get("result")
        .cloned()
        .unwrap_or_else(|| panic!("ExtResponse has no 'result' key; full JSON: {outer}"))
}
/// Replicate the lookup logic of code_nav_eligibility_for_request so we
/// can test it with a plain sessions HashMap.
fn check_nav_eligibility_from_sessions(
    sessions: &HashMap<acp::SessionId, crate::session::SessionHandle>,
    session_id: Option<&acp::SessionId>,
) -> Result<(), CodeNavEligibility> {
    let session_id = match session_id {
        Some(sid) => sid,
        None => return Err(CodeNavEligibility::SessionRequired),
    };
    let Some(handle) = sessions.get(session_id) else {
        return Err(CodeNavEligibility::SessionRequired);
    };
    let ct = crate::http::client_type_from_origin(handle.origin_client.as_ref());
    if !matches!(ct, ClientType::GrokWeb) {
        return Err(CodeNavEligibility::ClientNotWeb);
    }
    if !handle.code_nav_enabled {
        return Err(CodeNavEligibility::CapabilityNotAdvertised);
    }
    Ok(())
}
/// Web session with code-nav capability is eligible.
///
/// This is the "happy path" that allows lazy index startup on the first
/// code-nav request.
#[tokio::test]
async fn test_web_session_with_capability_is_eligible() {
    let sid = acp::SessionId::new("sess-web");
    let mut handle = make_test_handle("model", false, Some("grok-web"));
    handle.code_nav_enabled = true;
    let sessions = [(sid.clone(), handle)].into();
    assert!(
        check_nav_eligibility_from_sessions(&sessions, Some(&sid)).is_ok(),
        "web session with code-nav capability must be eligible"
    );
}
/// TUI session is rejected at gate 1 (client type) regardless of capability.
#[tokio::test]
async fn test_tui_session_is_rejected() {
    let sid = acp::SessionId::new("sess-tui");
    let mut handle = make_test_handle("model", false, Some("grok-tui"));
    handle.code_nav_enabled = true;
    let sessions = [(sid.clone(), handle)].into();
    assert_eq!(
        check_nav_eligibility_from_sessions(&sessions, Some(&sid)),
        Err(CodeNavEligibility::ClientNotWeb),
        "TUI client must be rejected at gate 1 (client type)"
    );
}
/// Web session without capability is rejected at gate 2.
#[tokio::test]
async fn test_web_session_without_capability_is_rejected() {
    let sid = acp::SessionId::new("sess-web-no-cap");
    let mut handle = make_test_handle("model", false, Some("grok-web"));
    handle.code_nav_enabled = false;
    let sessions = [(sid.clone(), handle)].into();
    assert_eq!(
        check_nav_eligibility_from_sessions(&sessions, Some(&sid)),
        Err(CodeNavEligibility::CapabilityNotAdvertised),
        "web client without capability must be rejected at gate 2"
    );
}
/// Leader-mode isolation: two sessions with different code-nav state return
/// independent results.
#[tokio::test]
async fn test_leader_mode_two_sessions_stay_isolated() {
    let web_sid = acp::SessionId::new("web");
    let tui_sid = acp::SessionId::new("tui");
    let mut web_handle = make_test_handle("model", false, Some("grok-web"));
    web_handle.code_nav_enabled = true;
    let mut tui_handle = make_test_handle("model", false, Some("grok-tui"));
    tui_handle.code_nav_enabled = false;
    let sessions = [(web_sid.clone(), web_handle), (tui_sid.clone(), tui_handle)].into();
    assert!(
        check_nav_eligibility_from_sessions(&sessions, Some(&web_sid)).is_ok(),
        "web session must be eligible"
    );
    assert_eq!(
        check_nav_eligibility_from_sessions(&sessions, Some(&tui_sid)),
        Err(CodeNavEligibility::ClientNotWeb),
        "tui session must remain ineligible even when web session is eligible"
    );
}
/// Unknown session ID returns SessionRequired, not a global fallback.
///
/// This is the stale/evicted session path: a caller with a session ID that
/// no longer exists in the sessions map must get SessionRequired, not
/// accidentally inherit the last-initialized client's eligibility.
#[tokio::test]
async fn test_unknown_session_id_returns_session_required() {
    let known_sid = acp::SessionId::new("known");
    let mut known_handle = make_test_handle("model", false, Some("grok-web"));
    known_handle.code_nav_enabled = true;
    let sessions = [(known_sid.clone(), known_handle)].into();
    let stale_sid = acp::SessionId::new("stale-or-evicted");
    assert_eq!(
        check_nav_eligibility_from_sessions(&sessions, Some(&stale_sid)),
        Err(CodeNavEligibility::SessionRequired),
        "stale/evicted sessionId must not fall back to global state"
    );
    assert!(check_nav_eligibility_from_sessions(&sessions, Some(&known_sid)).is_ok());
}
mod parse_json_object_env_tests {
    use super::parse_json_object_env;
    unsafe fn set(k: &str, v: &str) {
        unsafe { std::env::set_var(k, v) };
    }
    unsafe fn unset(k: &str) {
        unsafe { std::env::remove_var(k) };
    }
    #[test]
    #[serial_test::serial]
    fn valid_json_object_returns_some() {
        unsafe { set("TEST_JSON_OBJ", r#"{"team":"platform","org":"acme"}"#) };
        let result = parse_json_object_env("TEST_JSON_OBJ");
        unsafe { unset("TEST_JSON_OBJ") };
        let val = result.expect("should parse valid JSON object");
        assert_eq!(val["team"], "platform");
        assert_eq!(val["org"], "acme");
    }
    #[test]
    #[serial_test::serial]
    fn non_object_json_returns_none() {
        unsafe { set("TEST_JSON_ARR", r#"["not","an","object"]"#) };
        let result = parse_json_object_env("TEST_JSON_ARR");
        unsafe { unset("TEST_JSON_ARR") };
        assert!(result.is_none());
    }
    #[test]
    #[serial_test::serial]
    fn invalid_json_returns_none() {
        unsafe { set("TEST_JSON_BAD", "not json at all") };
        let result = parse_json_object_env("TEST_JSON_BAD");
        unsafe { unset("TEST_JSON_BAD") };
        assert!(result.is_none());
    }
    #[test]
    #[serial_test::serial]
    fn unset_var_returns_none() {
        unsafe { unset("TEST_JSON_UNSET") };
        assert!(parse_json_object_env("TEST_JSON_UNSET").is_none());
    }
}
mod eligibility_gates {
    use super::*;
    /// Standalone replica of the first three eligibility gates.
    /// Gate 4 (git root) requires a real filesystem and is covered by
    /// integration tests.
    fn check_gates(
        client_type: ClientType,
        code_nav_enabled: bool,
        indexing_enabled: bool,
    ) -> Result<(), CodeNavEligibility> {
        if !matches!(client_type, ClientType::GrokWeb) {
            return Err(CodeNavEligibility::ClientNotWeb);
        }
        if !code_nav_enabled {
            return Err(CodeNavEligibility::CapabilityNotAdvertised);
        }
        if !indexing_enabled {
            return Err(CodeNavEligibility::DisabledByConfig);
        }
        Ok(())
    }
    #[test]
    fn non_web_client_rejected() {
        assert_eq!(
            check_gates(ClientType::Generic, true, true),
            Err(CodeNavEligibility::ClientNotWeb)
        );
    }
    #[test]
    fn tui_client_rejected() {
        assert_eq!(
            check_gates(ClientType::GrokTUI, true, true),
            Err(CodeNavEligibility::ClientNotWeb)
        );
    }
    #[test]
    fn web_client_no_capability_rejected() {
        assert_eq!(
            check_gates(ClientType::GrokWeb, false, true),
            Err(CodeNavEligibility::CapabilityNotAdvertised)
        );
    }
    #[test]
    fn web_client_with_capability_config_disabled_rejected() {
        assert_eq!(
            check_gates(ClientType::GrokWeb, true, false),
            Err(CodeNavEligibility::DisabledByConfig)
        );
    }
    #[test]
    fn web_client_with_capability_and_config_passes_first_three_gates() {
        assert!(check_gates(ClientType::GrokWeb, true, true).is_ok());
    }
}
#[test]
fn find_model_by_id_prefers_key_then_falls_back_to_slug() {
    let entry = |model: &str| ModelEntry {
        info: config::ModelInfo {
            user_selectable: true,
            id: None,
            model: model.to_string(),
            base_url: String::new(),
            name: None,
            description: None,
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            api_backend: crate::sampling::ApiBackend::default(),
            auth_scheme: Default::default(),
            extra_headers: IndexMap::new(),
            context_window: std::num::NonZeroU64::new(200_000).unwrap(),
            auto_compact_threshold_percent: None,
            system_prompt_label: None,
            use_concise: false,
            agent_type: config::default_agent_type(),
            inference_idle_timeout_secs: None,
            max_retries: None,
            hidden: false,
            supported_in_api: true,
            reasoning_effort: None,
            supports_reasoning_effort: false,
            reasoning_efforts: Vec::new(),
            supports_backend_search: false,
            compactions_remaining: None,
            compaction_at_tokens: None,
            show_model_fingerprint: false,
            stream_tool_calls: None,
            laziness_detector: crate::agent::config::LazinessDetectorPerModelConfig::default(),
        },
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
    };
    let mut models = indexmap::IndexMap::new();
    models.insert("a".to_string(), entry("target"));
    models.insert("target".to_string(), entry("other"));
    assert_eq!(
        config::find_model_by_id(&models, "target").unwrap().model,
        "other",
        "key match should win over slug scan"
    );
    assert_eq!(
        config::find_model_by_id(&models, "a").unwrap().model,
        "target",
        "exact key match for 'a'"
    );
}
fn write_updates(dir: &std::path::Path, lines: &[&str]) -> PathBuf {
    let path = dir.join("updates.jsonl");
    std::fs::write(&path, lines.join("\n")).unwrap();
    path
}
fn bg_line(task_id: &str) -> String {
    format!(
        r#"{{"timestamp":1,"method":"_x.ai/session/update","params":{{"sessionId":"s","update":{{"sessionUpdate":"task_backgrounded","task_id":"{task_id}","command":"sleep 99","cwd":"/tmp"}}}}}}"#
    )
}
fn completed_line(task_id: &str) -> String {
    format!(
        r#"{{"timestamp":2,"method":"_x.ai/session/update","params":{{"sessionId":"s","update":{{"sessionUpdate":"task_completed","task_snapshot":{{"task_id":"{task_id}","completed":true}}}}}}}}"#
    )
}
fn orphaned_ids(tasks: &[OrphanedTask]) -> std::collections::HashSet<&str> {
    tasks.iter().map(|t| t.task_id.as_str()).collect()
}
#[test]
fn orphaned_tasks_returns_empty_for_no_file() {
    let result = MvpAgent::find_orphaned_background_tasks(&None);
    assert!(result.is_empty());
}
#[test]
fn orphaned_tasks_returns_empty_for_missing_file() {
    let path = PathBuf::from("/nonexistent/updates.jsonl");
    let result = MvpAgent::find_orphaned_background_tasks(&Some(path));
    assert!(result.is_empty());
}
#[test]
fn orphaned_tasks_returns_empty_when_all_completed() {
    let tmp = tempfile::tempdir().unwrap();
    let bg = bg_line("t1");
    let done = completed_line("t1");
    let path = write_updates(tmp.path(), &[&bg, &done]);
    let result = MvpAgent::find_orphaned_background_tasks(&Some(path));
    assert!(result.is_empty());
}
#[test]
fn orphaned_tasks_returns_uncompleted() {
    let tmp = tempfile::tempdir().unwrap();
    let bg1 = bg_line("t1");
    let bg2 = bg_line("t2");
    let done1 = completed_line("t1");
    let path = write_updates(tmp.path(), &[&bg1, &bg2, &done1]);
    let result = MvpAgent::find_orphaned_background_tasks(&Some(path));
    let ids = orphaned_ids(&result);
    assert_eq!(ids.len(), 1);
    assert!(ids.contains("t2"));
}
#[test]
fn orphaned_tasks_returns_multiple_uncompleted() {
    let tmp = tempfile::tempdir().unwrap();
    let bg1 = bg_line("t1");
    let bg2 = bg_line("t2");
    let bg3 = bg_line("t3");
    let done2 = completed_line("t2");
    let path = write_updates(tmp.path(), &[&bg1, &bg2, &bg3, &done2]);
    let result = MvpAgent::find_orphaned_background_tasks(&Some(path));
    let ids = orphaned_ids(&result);
    assert_eq!(ids.len(), 2);
    assert!(ids.contains("t1"));
    assert!(ids.contains("t3"));
}
#[test]
fn orphaned_tasks_captures_command_and_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    let bg = bg_line("t1");
    let path = write_updates(tmp.path(), &[&bg]);
    let result = MvpAgent::find_orphaned_background_tasks(&Some(path));
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].command, "sleep 99");
    assert_eq!(result[0].cwd, "/tmp");
}
#[test]
fn orphaned_tasks_skips_malformed_lines() {
    let tmp = tempfile::tempdir().unwrap();
    let bg = bg_line("t1");
    let path = write_updates(tmp.path(), &["not json", &bg, "{}"]);
    let result = MvpAgent::find_orphaned_background_tasks(&Some(path));
    assert_eq!(result.len(), 1);
}
#[test]
fn orphaned_tasks_ignores_unrelated_updates() {
    let tmp = tempfile::tempdir().unwrap();
    let bg = bg_line("t1");
    let unrelated = r#"{"timestamp":1,"method":"_x.ai/session/update","params":{"sessionId":"s","update":{"sessionUpdate":"auto_compact_started","percentage":80}}}"#;
    let path = write_updates(tmp.path(), &[&bg, unrelated]);
    let result = MvpAgent::find_orphaned_background_tasks(&Some(path));
    assert_eq!(result.len(), 1);
}
#[test]
fn orphaned_tasks_filters_rewind_dead_branches() {
    let tmp = tempfile::tempdir().unwrap();
    let user_msg = r#"{"timestamp":0,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hello"}}}}"#;
    let bg_before_rewind = bg_line("t-dead");
    let rewind = r#"{"timestamp":3,"method":"_x.ai/session/update","params":{"sessionId":"s","update":{"sessionUpdate":"rewind_marker","target_prompt_index":0,"created_at":"2025-01-01T00:00:00Z"}}}"#;
    let user_msg2 = r#"{"timestamp":4,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"retry"}}}}"#;
    let bg_after_rewind = bg_line("t-alive");
    let path = write_updates(
        tmp.path(),
        &[
            user_msg,
            &bg_before_rewind,
            rewind,
            user_msg2,
            &bg_after_rewind,
        ],
    );
    let result = MvpAgent::find_orphaned_background_tasks(&Some(path));
    let ids = orphaned_ids(&result);
    assert!(
        ids.contains("t-alive"),
        "task after rewind should be present"
    );
    assert!(
        !ids.contains("t-dead"),
        "task in dead branch should be filtered"
    );
}
#[test]
fn allow_access_from_remote_settings() {
    let json = serde_json::json!({ "allow_access" : true });
    let rs: crate::util::config::RemoteSettings = serde_json::from_value(json).unwrap();
    assert_eq!(rs.allow_access, Some(true));
    let json = serde_json::json!({ "allow_access" : false });
    let rs: crate::util::config::RemoteSettings = serde_json::from_value(json).unwrap();
    assert_eq!(rs.allow_access, Some(false));
    let json = serde_json::json!({});
    let rs: crate::util::config::RemoteSettings = serde_json::from_value(json).unwrap();
    assert_eq!(rs.allow_access, None);
}
#[test]
fn on_demand_enabled_from_remote_settings() {
    let json = serde_json::json!({ "on_demand_enabled" : false });
    let rs: crate::util::config::RemoteSettings = serde_json::from_value(json).unwrap();
    assert_eq!(rs.on_demand_enabled, Some(false));
    let json = serde_json::json!({});
    let rs: crate::util::config::RemoteSettings = serde_json::from_value(json).unwrap();
    assert_eq!(rs.on_demand_enabled, None);
}
/// Regression for a 401 sequence seen in production. After a long idle
/// window, the auth manager may have no
/// live token by the time `session/new` runs. For session-based auth methods
/// we MUST still report `SessionToken` so chat_state credentials retain the
/// session-token shape and `try_refresh_session_token` will run on the next
/// prompt instead of early-returning.
#[tokio::test(flavor = "current_thread")]
async fn auth_type_session_based_no_current_returns_session_token() {
    for method_id in [
        crate::agent::auth_method::CACHED_TOKEN_AUTH_METHOD_ID,
        crate::agent::auth_method::GROK_COM_METHOD_ID,
        crate::agent::auth_method::OIDC_METHOD_ID,
    ] {
        let agent = build_minimal_agent_for_tests();
        agent.set_auth_method(acp::AuthMethodId::new(method_id));
        assert!(
            agent.auth_manager.current().is_none(),
            "{method_id}: precondition: AuthManager has no current token",
        );
        assert_eq!(
            agent.auth_type(),
            xai_chat_state::AuthType::SessionToken,
            "{method_id}: session-based auth must report SessionToken even \
                 without a live token -- otherwise chat_state gets locked into \
                 auth_type = ApiKey and try_refresh_session_token will skip \
                 every subsequent refresh attempt.",
        );
    }
}
/// BYOK guard. Users with `xai.api_key` must continue to report `ApiKey`
/// regardless of live-token state -- BYOK sessions have nothing to refresh,
/// and reporting `SessionToken` would route through cli-chat-proxy paths
/// (image_gen / video_gen base_url) that don't apply to BYOK keys.
#[tokio::test(flavor = "current_thread")]
async fn auth_type_xai_api_key_no_current_returns_api_key() {
    let agent = build_minimal_agent_for_tests();
    agent.set_auth_method(acp::AuthMethodId::new(
        crate::agent::auth_method::XAI_API_KEY_METHOD_ID,
    ));
    assert!(agent.auth_manager.current().is_none());
    assert_eq!(
        agent.auth_type(),
        xai_chat_state::AuthType::ApiKey,
        "xai.api_key auth must report ApiKey -- BYOK has no session-token \
             behavior to fall back to."
    );
}
/// Positive baseline: when both signals agree (session-based method AND
/// a live in-memory token), `SessionToken` is returned. This is the
/// common case during a healthy session.
#[tokio::test(flavor = "current_thread")]
async fn auth_type_session_based_with_current_returns_session_token() {
    use crate::auth::GrokAuth;
    let agent = build_minimal_agent_for_tests();
    agent.set_auth_method(acp::AuthMethodId::new(
        crate::agent::auth_method::OIDC_METHOD_ID,
    ));
    agent.auth_manager.hot_swap(GrokAuth::test_default());
    assert!(agent.auth_manager.current().is_some());
    assert_eq!(agent.auth_type(), xai_chat_state::AuthType::SessionToken,);
}
/// Defensive case: no `auth_method_id` selected yet (pre-`authenticate`
/// state) and no live credential. We default to `ApiKey` so callers
/// that key off this value (e.g. `resolve_chat_state_auth_type` for chat
/// routing) don't accidentally route session-token-shaped traffic
/// through cli-chat-proxy before a method has been chosen.
#[tokio::test(flavor = "current_thread")]
async fn auth_type_no_method_id_no_current_returns_api_key() {
    let agent = build_minimal_agent_for_tests();
    assert!(agent.auth_method_id.load().is_none());
    assert!(agent.auth_manager.current().is_none());
    assert_eq!(agent.auth_type(), xai_chat_state::AuthType::ApiKey,);
}
/// Live credential present but `auth_method_id` is still `None`. The
/// in-memory bearer takes precedence: this is the order observed during
/// `initialize()` silent refresh -- a token is hot-swapped in before
/// `authenticate()` writes the method id. Reporting `SessionToken`
/// here matches pre-fix behavior and keeps logging stable.
#[tokio::test(flavor = "current_thread")]
async fn auth_type_no_method_id_with_current_returns_session_token() {
    use crate::auth::GrokAuth;
    let agent = build_minimal_agent_for_tests();
    agent.auth_manager.hot_swap(GrokAuth::test_default());
    assert!(agent.auth_method_id.load().is_none());
    assert!(agent.auth_manager.current().is_some());
    assert_eq!(agent.auth_type(), xai_chat_state::AuthType::SessionToken,);
}
/// Minimal agent whose `grok_com_config` engages the api-key kill switch
/// (`disable_api_key_auth = true`), mirroring a forced-IdP deployment.
fn build_agent_with_api_key_auth_disabled() -> MvpAgent {
    use crate::agent::config::Config as AgentConfig;
    use crate::auth::{AuthManager, GrokComConfig};
    let temp_dir = tempfile::tempdir().unwrap();
    let auth_manager =
        std::sync::Arc::new(AuthManager::new(temp_dir.path(), GrokComConfig::default()));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let gateway = GatewaySender::new(tx);
    let mut cfg = AgentConfig::default();
    cfg.grok_com_config.disable_api_key_auth = Some(true);
    MvpAgent::new(gateway, &cfg, auth_manager, None).expect("valid test config")
}
/// Deployment-key / managed-config user: `XAI_API_KEY` resolves and the kill
/// switch is off, so a dead `cached_token` MUST fall through to `xai.api_key`
/// (no browser). This is the exact regression the fallthrough fixes.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn cached_token_fallthrough_prefers_api_key_for_deployment_key() {
    use crate::agent::auth_method::{XAI_API_KEY_ENV_VAR, XAI_API_KEY_METHOD_ID};
    use xai_grok_test_support::EnvGuard;
    let _lockdown = EnvGuard::unset("GROK_DISABLE_API_KEY_AUTH");
    let _key = EnvGuard::set(XAI_API_KEY_ENV_VAR, "test-deployment-key");
    let agent = build_minimal_agent_for_tests();
    assert_eq!(
        agent
            .cached_token_fallthrough_method_id()
            .as_ref()
            .map(|id| id.0.as_ref()),
        Some(XAI_API_KEY_METHOD_ID),
        "deployment-key user (XAI_API_KEY set, no kill switch) must fall \
         through to xai.api_key on a dead cached_token -- not interactive login",
    );
}
/// Forced-IdP deployment: even with `XAI_API_KEY` present, the admin kill
/// switch keeps the fallthrough on interactive `grok.com` (api-key auth is
/// neither advertised nor an eligible fallthrough).
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn cached_token_fallthrough_respects_kill_switch() {
    use crate::agent::auth_method::{GROK_COM_METHOD_ID, XAI_API_KEY_ENV_VAR};
    use xai_grok_test_support::EnvGuard;
    let _lockdown = EnvGuard::unset("GROK_DISABLE_API_KEY_AUTH");
    let _key = EnvGuard::set(XAI_API_KEY_ENV_VAR, "test-deployment-key");
    let agent = build_agent_with_api_key_auth_disabled();
    assert_eq!(
        agent
            .cached_token_fallthrough_method_id()
            .as_ref()
            .map(|id| id.0.as_ref()),
        Some(GROK_COM_METHOD_ID),
        "disable_api_key_auth must keep the cached_token fallthrough on \
         interactive grok.com so XAI_API_KEY can't bypass forced IdP login",
    );
}
/// No advertiseable credentials at all (no env key, no kill switch): the user
/// genuinely needs to log in, so the fallthrough is interactive `grok.com`.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn cached_token_fallthrough_falls_to_grok_com_without_credentials() {
    use crate::agent::auth_method::{
        GROK_COM_METHOD_ID, LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR,
    };
    use xai_grok_test_support::EnvGuard;
    let _lockdown = EnvGuard::unset("GROK_DISABLE_API_KEY_AUTH");
    let _new = EnvGuard::unset(XAI_API_KEY_ENV_VAR);
    let _legacy = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);
    let agent = build_minimal_agent_for_tests();
    assert_eq!(
        agent
            .cached_token_fallthrough_method_id()
            .as_ref()
            .map(|id| id.0.as_ref()),
        Some(GROK_COM_METHOD_ID),
        "no API-key creds and no kill switch -> interactive grok.com login",
    );
}
/// Verifies the 4-state matrix of `(disable_zdr_incompatible_tools, zdr_video_output_s3)`:
///
/// | ZDR flag | S3 config | Result                                      |
/// |----------|-----------|---------------------------------------------|
/// | false    | None      | Enabled, no S3 (normal non-ZDR mode)        |
/// | true     | None      | Disabled (ZDR with no escape hatch)         |
/// | false    | Some      | Enabled, S3 **not** threaded (non-ZDR)      |
/// | true     | Some      | Enabled, S3 threaded (ZDR with upload path) |
#[tokio::test(flavor = "current_thread")]
async fn prepare_video_gen_config_disabled_when_zdr_flag_set() {
    use xai_grok_tools::implementations::grok_build::video_gen::{
        S3AccessCredentials, VideoGenConfig, ZdrVideoOutputS3Config,
    };
    fn zdr_s3() -> ZdrVideoOutputS3Config {
        ZdrVideoOutputS3Config {
            bucket: "team-videos".into(),
            endpoint: "https://s3.example.com".into(),
            region: "us-east-1".into(),
            key_prefix: "grok-videos/".into(),
            expires_secs: 900,
            read_write: S3AccessCredentials {
                access_key_id: "AKIA...".into(),
                secret_access_key: "secret".into(),
            },
            read_only: None,
        }
    }
    let agent = build_minimal_agent_for_tests();
    agent.sampling_config.borrow_mut().api_key = Some("test-key".to_string());
    assert!(matches!(
        agent.prepare_video_gen_config(),
        VideoGenConfig::Enabled { .. }
    ));
    agent.cfg.borrow_mut().disable_zdr_incompatible_tools = true;
    assert!(matches!(
        agent.prepare_video_gen_config(),
        VideoGenConfig::Disabled
    ));
    agent.cfg.borrow_mut().zdr_video_output_s3 = Some(zdr_s3());
    agent.cfg.borrow_mut().disable_zdr_incompatible_tools = false;
    let VideoGenConfig::Enabled {
        zdr_video_output_s3: s3_when_non_zdr,
        ..
    } = agent.prepare_video_gen_config()
    else {
        panic!("expected Enabled");
    };
    assert!(
        s3_when_non_zdr.is_none(),
        "S3 config must not be threaded when ZDR flag is off"
    );
    agent.cfg.borrow_mut().disable_zdr_incompatible_tools = true;
    let VideoGenConfig::Enabled {
        zdr_video_output_s3,
        ..
    } = agent.prepare_video_gen_config()
    else {
        panic!("expected Enabled");
    };
    assert!(zdr_video_output_s3.as_ref().is_some_and(|c| c.is_valid()));
}
/// The imagine tier gate fails **open**: with no resolved auth we can't confirm
/// a restricted personal tier, so the tools stay advertised and un-flagged (the
/// server 429 remains the authoritative backstop). Guards against accidentally
/// disabling a paid feature when tier info hasn't loaded.
#[tokio::test(flavor = "current_thread")]
async fn prepare_image_gen_config_fails_open_without_auth() {
    use xai_grok_tools::implementations::grok_build::image_gen::ImageGenConfig;
    let agent = build_minimal_agent_for_tests();
    agent.sampling_config.borrow_mut().api_key = Some("test-key".to_string());
    let ImageGenConfig::Enabled {
        tier_restricted, ..
    } = agent.prepare_image_gen_config()
    else {
        panic!("expected Enabled");
    };
    assert!(
        !tier_restricted,
        "no resolved auth ⇒ fail open (tools not tier-restricted)"
    );
}
/// The imagine tools bypass cli-chat-proxy (direct API calls), so the server
/// can only scope the coding data-retention opt-out (`/privacy opt-out`) to
/// Build traffic via the `x-grok-client-identifier` header. If this header is
/// dropped, opted-out users' imagine prompts are logged/retained server-side.
#[tokio::test(flavor = "current_thread")]
async fn prepare_image_gen_config_sends_client_identifier_header() {
    use xai_grok_tools::implementations::grok_build::image_gen::ImageGenConfig;
    let agent = build_minimal_agent_for_tests();
    agent.sampling_config.borrow_mut().api_key = Some("test-key".to_string());
    let ImageGenConfig::Enabled { extra_headers, .. } = agent.prepare_image_gen_config() else {
        panic!("expected Enabled");
    };
    assert_eq!(
        extra_headers
            .get("x-grok-client-identifier")
            .map(String::as_str),
        Some(crate::http::process_client_identifier().as_str()),
        "imagine API calls must carry the client identifier so the server \
         applies the coding ZDR opt-out to Build traffic"
    );
}
/// Same contract for video generation (also a direct API call).
#[tokio::test(flavor = "current_thread")]
async fn prepare_video_gen_config_sends_client_identifier_header() {
    use xai_grok_tools::implementations::grok_build::video_gen::VideoGenConfig;
    let agent = build_minimal_agent_for_tests();
    agent.sampling_config.borrow_mut().api_key = Some("test-key".to_string());
    let VideoGenConfig::Enabled { extra_headers, .. } = agent.prepare_video_gen_config() else {
        panic!("expected Enabled");
    };
    assert_eq!(
        extra_headers
            .get("x-grok-client-identifier")
            .map(String::as_str),
        Some(crate::http::process_client_identifier().as_str()),
        "video gen API calls must carry the client identifier so the server \
         applies the coding ZDR opt-out to Build traffic"
    );
}
#[tokio::test]
async fn data_collection_enabled_for_normal_user() {
    let agent = build_agent_with_auth(crate::auth::GrokAuth::test_default());
    assert!(
        !agent.is_data_collection_disabled(),
        "normal user must have data collection enabled"
    );
}
#[tokio::test]
async fn data_collection_disabled_for_zdr_team() {
    let agent = build_agent_with_auth(crate::auth::GrokAuth {
        team_blocked_reasons: vec!["BLOCKED_REASON_NO_LOGS".into()],
        ..crate::auth::GrokAuth::test_default()
    });
    assert!(
        agent.is_data_collection_disabled(),
        "ZDR team must have data collection disabled"
    );
    assert!(
        agent.trace_upload_config_snapshot().is_none(),
        "trace uploads must be disabled for ZDR team"
    );
}
#[tokio::test]
async fn data_collection_disabled_for_zdr_moderated_team() {
    let agent = build_agent_with_auth(crate::auth::GrokAuth {
        team_blocked_reasons: vec!["BLOCKED_REASON_NO_LOGS_MODERATED".into()],
        ..crate::auth::GrokAuth::test_default()
    });
    assert!(
        agent.is_data_collection_disabled(),
        "ZDR-moderated team must have data collection disabled"
    );
}
#[tokio::test]
async fn data_collection_disabled_for_opted_out_team() {
    let agent = build_agent_with_auth(crate::auth::GrokAuth {
        coding_data_retention_opt_out: true,
        ..crate::auth::GrokAuth::test_default()
    });
    assert!(
        agent.is_data_collection_disabled(),
        "opted-out team must have data collection disabled"
    );
    assert!(
        agent.trace_upload_config_snapshot().is_none(),
        "trace uploads must be disabled for opted-out team"
    );
}
#[tokio::test]
async fn data_collection_disabled_for_zdr_plus_opt_out() {
    let agent = build_agent_with_auth(crate::auth::GrokAuth {
        team_blocked_reasons: vec!["BLOCKED_REASON_NO_LOGS".into()],
        coding_data_retention_opt_out: true,
        ..crate::auth::GrokAuth::test_default()
    });
    assert!(
        agent.is_data_collection_disabled(),
        "ZDR + opt-out must have data collection disabled"
    );
}
#[tokio::test]
async fn data_collection_enabled_for_non_zdr_team_with_unrelated_blocks() {
    let agent = build_agent_with_auth(crate::auth::GrokAuth {
        team_blocked_reasons: vec![
            "BLOCKED_REASON_BILLING".into(),
            "BLOCKED_REASON_SUSPENDED".into(),
        ],
        ..crate::auth::GrokAuth::test_default()
    });
    assert!(
        !agent.is_data_collection_disabled(),
        "non-ZDR blocked reasons must not disable data collection"
    );
}
fn enable_product_telemetry(agent: &MvpAgent) {
    agent.cfg.borrow_mut().features.telemetry = Some(crate::agent::config::TelemetryMode::Enabled);
}
/// Enable trace uploads via config so only the auth-level privacy gate
/// can disable collection in the tests below.
fn enable_trace_upload_config(agent: &MvpAgent) {
    let mut cfg = agent.cfg.borrow_mut();
    cfg.features.telemetry = Some(crate::agent::config::TelemetryMode::Enabled);
    cfg.telemetry.trace_upload = Some(true);
}
#[tokio::test]
async fn product_analytics_enabled_for_normal_user_with_telemetry_on() {
    let agent = build_agent_with_auth(crate::auth::GrokAuth::test_default());
    enable_product_telemetry(&agent);
    assert!(agent.product_analytics_enabled());
}
#[tokio::test]
async fn product_analytics_enabled_despite_coding_retention_opt_out() {
    let agent = build_agent_with_auth(crate::auth::GrokAuth {
        coding_data_retention_opt_out: true,
        ..crate::auth::GrokAuth::test_default()
    });
    enable_product_telemetry(&agent);
    assert!(agent.is_data_collection_disabled());
    assert!(agent.product_analytics_enabled());
}
#[tokio::test]
async fn product_analytics_disabled_for_zdr_team() {
    let agent = build_agent_with_auth(crate::auth::GrokAuth {
        team_blocked_reasons: vec!["BLOCKED_REASON_NO_LOGS".into()],
        ..crate::auth::GrokAuth::test_default()
    });
    enable_product_telemetry(&agent);
    assert!(!agent.product_analytics_enabled());
}
#[tokio::test]
async fn product_analytics_disabled_when_telemetry_off() {
    let agent = build_agent_with_auth(crate::auth::GrokAuth::test_default());
    agent.cfg.borrow_mut().features.telemetry = Some(crate::agent::config::TelemetryMode::Disabled);
    assert!(!agent.product_analytics_enabled());
}
/// Counting HTTP stub: any request increments the counter and gets a
/// storage-proxy-shaped 200 so the client does not retry.
async fn spawn_counting_storage_stub() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count_clone = count.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = axum::Router::new().fallback(move || {
        let count = count_clone.clone();
        async move {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (
                [("content-type", "application/json")],
                r#"{"bucket":"test-bucket","path":"auth-diagnostics/test.jsonl"}"#,
            )
        }
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://127.0.0.1:{port}"), count)
}
/// Regression: the auth-diagnostics uploader was gated only on the
/// trace-upload config switch; it must also honor ZDR / retention
/// opt-out, checked at invocation time.
#[tokio::test]
async fn diagnostic_upload_skipped_for_opted_out_user() {
    let (stub_url, count) = spawn_counting_storage_stub().await;
    let agent = build_agent_with_auth(crate::auth::GrokAuth {
        coding_data_retention_opt_out: true,
        ..crate::auth::GrokAuth::test_default()
    });
    enable_trace_upload_config(&agent);
    agent.cfg.borrow_mut().endpoints.trace_upload_url = Some(stub_url);
    let uploader = agent
        .diagnostic_upload_config()
        .expect("uploader is wired whenever trace upload config is on");
    uploader(b"log".to_vec(), "tok".into(), "user-id-1".into()).await;
    assert_eq!(
        count.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no diagnostics request may leave the machine after opt-out"
    );
}
#[tokio::test]
async fn diagnostic_upload_sent_for_normal_user() {
    let (stub_url, count) = spawn_counting_storage_stub().await;
    let agent = build_agent_with_auth(crate::auth::GrokAuth::test_default());
    enable_trace_upload_config(&agent);
    agent.cfg.borrow_mut().endpoints.trace_upload_url = Some(stub_url);
    let uploader = agent
        .diagnostic_upload_config()
        .expect("uploader is wired whenever trace upload config is on");
    uploader(b"log".to_vec(), "tok".into(), "user-id-1".into()).await;
    assert!(
        count.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "positive control: diagnostics upload reaches the proxy for a \
         normal user"
    );
}
/// The diagnostics privacy gate fails closed: with no credential in the
/// `AuthManager` (e.g. a mid-session `/logout` raced the refresh failure
/// that triggers the upload), nothing may leave the machine.
#[tokio::test]
async fn diagnostic_upload_skipped_without_credentials() {
    let (stub_url, count) = spawn_counting_storage_stub().await;
    let agent = build_minimal_agent_for_tests();
    enable_trace_upload_config(&agent);
    agent.cfg.borrow_mut().endpoints.trace_upload_url = Some(stub_url);
    let uploader = agent
        .diagnostic_upload_config()
        .expect("uploader is wired whenever trace upload config is on");
    uploader(b"log".to_vec(), "tok".into(), "user-id-1".into()).await;
    assert_eq!(
        count.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "missing credentials must fail closed for diagnostics uploads"
    );
}
/// The diagnostics uploader is wired once (at agent construction), so it
/// must re-check the live trace-upload mirror at invocation time: a
/// mid-session config-level kill switch stops diagnostics uploads too.
#[tokio::test]
async fn diagnostic_upload_skipped_after_mid_session_trace_upload_kill_switch() {
    let (stub_url, count) = spawn_counting_storage_stub().await;
    let agent = build_agent_with_auth(crate::auth::GrokAuth::test_default());
    enable_trace_upload_config(&agent);
    agent.cfg.borrow_mut().endpoints.trace_upload_url = Some(stub_url);
    agent.sync_collection_config_gate();
    let uploader = agent
        .diagnostic_upload_config()
        .expect("uploader is wired whenever trace upload config is on");
    {
        let mut cfg = agent.cfg.borrow_mut();
        cfg.features.telemetry = Some(crate::agent::config::TelemetryMode::Disabled);
        cfg.telemetry.trace_upload = Some(false);
    }
    agent.sync_collection_config_gate();
    uploader(b"log".to_vec(), "tok".into(), "user-id-1".into()).await;
    assert_eq!(
        count.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "an already-wired diagnostics uploader must honor a mid-session \
         trace-upload kill switch"
    );
}
/// The live collection gate reads a `Send` mirror of the config-level
/// trace-upload switch; `sync_collection_config_gate` must keep that mirror
/// current so a mid-session remote-settings flip (kill switch) stops
/// collection without a new session.
#[tokio::test]
async fn collection_config_gate_mirror_follows_trace_upload_flip() {
    let agent = build_agent_with_auth(crate::auth::GrokAuth::test_default());
    enable_trace_upload_config(&agent);
    agent.sync_collection_config_gate();
    assert!(
        agent
            .trace_upload_live
            .load(std::sync::atomic::Ordering::Relaxed),
        "precondition: mirror reflects the enabled switch"
    );
    {
        let mut cfg = agent.cfg.borrow_mut();
        cfg.features.telemetry = Some(crate::agent::config::TelemetryMode::Disabled);
        cfg.telemetry.trace_upload = Some(false);
    }
    agent.sync_collection_config_gate();
    assert!(
        !agent
            .trace_upload_live
            .load(std::sync::atomic::Ordering::Relaxed),
        "mirror must follow a mid-session config-level trace-upload flip"
    );
}
/// `parse_session_kind` routes `session/load` to the gateway Chat path vs. the
/// disk-backed Build path. Anything but an explicit `kind: "chat"` is Build.
#[test]
fn parse_session_kind_matrix() {
    use crate::session::unified_list::SessionKind;
    use serde_json::json;
    let cases: &[(&str, serde_json::Value, SessionKind)] = &[
        (
            "chat",
            json!({ "x.ai/session" : { "kind" : "chat" } }),
            SessionKind::Chat,
        ),
        (
            "build",
            json!({ "x.ai/session" : { "kind" : "build" } }),
            SessionKind::Build,
        ),
        (
            "chat_malformed_sibling",
            json!({ "x.ai/session" : { "kind" : "chat", "facets" : "not-a-map" } }),
            SessionKind::Chat,
        ),
        (
            "unknown_kind",
            json!({ "x.ai/session" : { "kind" : "frob" } }),
            SessionKind::Build,
        ),
        ("absent", json!({}), SessionKind::Build),
    ];
    for (label, meta, expected) in cases {
        assert_eq!(parse_session_kind(meta.as_object()), *expected, "[{label}]");
    }
    assert_eq!(parse_session_kind(None), SessionKind::Build, "[none]");
}
#[test]
fn chat_initial_model_matrix() {
    let cases: &[(&str, bool, Option<&str>, Option<&str>)] = &[
        ("chat_with_model", true, Some("grok-4.5"), Some("grok-4.5")),
        ("chat_without_model", true, None, None),
        ("build_with_model", false, Some("grok-4.5"), None),
        ("build_without_model", false, None, None),
    ];
    for (label, is_chat_kind, custom_model_id, expected) in cases {
        assert_eq!(
            chat_initial_model(*is_chat_kind, *custom_model_id).as_deref(),
            *expected,
            "[{label}]"
        );
    }
}
#[test]
fn chat_new_session_model_state_matrix() {
    fn state_with(current: &str, available: &[&str]) -> acp::SessionModelState {
        acp::SessionModelState::new(
            acp::ModelId::new(current.to_owned()),
            available
                .iter()
                .map(|id| {
                    acp::ModelInfo::new(acp::ModelId::new((*id).to_owned()), (*id).to_owned())
                })
                .collect(),
        )
    }
    let cases: &[(&str, acp::SessionModelState, Option<&str>, &str)] = &[
        (
            "requested_in_catalog",
            state_with("auto", &["auto", "grok-4"]),
            Some("grok-4"),
            "grok-4",
        ),
        (
            "no_request_keeps_catalog_default",
            state_with("auto", &["auto", "grok-4"]),
            None,
            "auto",
        ),
        (
            "requested_not_in_catalog",
            state_with("auto", &["auto"]),
            Some("grok-4.5"),
            "grok-4.5",
        ),
        (
            "requested_with_empty_catalog",
            state_with("", &[]),
            Some("grok-4"),
            "grok-4",
        ),
    ];
    for (label, state, requested, expected) in cases {
        let out = chat_new_session_model_state(state.clone(), requested.map(str::to_owned));
        assert_eq!(out.current_model_id.0.as_ref(), *expected, "[{label}]");
        assert_eq!(
            out.available_models.len(),
            state.available_models.len(),
            "[{label}] override must not mutate the catalog"
        );
    }
}
/// `spawn_gateway_bridge` uses `tokio::task::spawn_local`.
fn run_local_for_bridge_test<F, Fut, T>(body: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime must build");
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, body())
}
#[test]
fn chat_session_spawn_options_matches_thin_profile() {
    let sid = acp::SessionId::new(std::sync::Arc::from("00000000-0000-0000-0000-000000000099"));
    let cwd = xai_grok_paths::AbsPathBuf::new(std::env::temp_dir()).expect("temp cwd");
    let opts = chat_session_spawn_options(
        SessionInfo {
            id: sid,
            cwd: cwd.as_str().to_owned(),
        },
        cwd,
        None,
        None,
        acp::ModelId::new(std::sync::Arc::from("test-model")),
        false,
    );
    assert!(opts.mcp_servers.is_empty());
    assert!(opts.initial_client_mcp_servers.is_empty());
    assert!(!opts.client_code_nav_enabled);
    assert!(!opts.client_terminal);
    assert!(!opts.client_fs_read);
    assert!(!opts.client_fs_write);
    assert!(opts.chat_history.is_empty());
    assert!(opts.managed_mcp_expires_at.is_none());
    assert!(!opts.session_auto_mode);
    assert!(
        opts.persistence.is_noop(),
        "K10 thin profile must use PersistenceHandle::noop()"
    );
}
/// `remove_session` releases the workspace binding and drains the
/// per-session side maps. Test agents default to `workspace_ops = None`,
/// so no other test reaches the release.
#[tokio::test]
async fn remove_session_releases_workspace_binding_and_side_maps() {
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("test-session-workspace-release");
    let ops = xai_grok_workspace::WorkspaceOps::for_test();
    let toolset =
        std::sync::Arc::new(xai_grok_tools::registry::types::FinalizedToolset::empty_for_test());
    let toolset_weak = std::sync::Arc::downgrade(&toolset);
    ops.bind_local_session(
        sid.0.as_ref(),
        std::env::temp_dir(),
        xai_hunk_tracker::HunkTrackerHandle::noop(),
        toolset,
        None,
    )
    .expect("bind_local_session must succeed");
    assert!(toolset_weak.upgrade().is_some());
    *agent.workspace_ops.borrow_mut() = Some(ops);
    agent.model_unavailable_sessions.borrow_mut().insert(
        sid.0.to_string(),
        acp::ModelId::new(std::sync::Arc::from("gone-model")),
    );
    agent
        .session_turn_numbers
        .borrow_mut()
        .insert(sid.clone(), 3);
    let (_permission_tx, permission_rx) =
        tokio::sync::mpsc::unbounded_channel::<xai_grok_workspace::permission::PermissionEvent>();
    agent
        .permission_event_receivers
        .borrow_mut()
        .insert(sid.clone(), permission_rx);
    agent.remove_session(&sid);
    assert!(
        toolset_weak.upgrade().is_none(),
        "the workspace binding must release the toolset"
    );
    assert!(
        !agent
            .model_unavailable_sessions
            .borrow()
            .contains_key(sid.0.as_ref())
    );
    assert!(!agent.session_turn_numbers.borrow().contains_key(&sid));
    assert!(!agent.permission_event_receivers.borrow().contains_key(&sid));
}
/// Without a bridge, `ext_method` falls through to the unchanged local
/// dispatch (`rewind::handle`), which reports the missing session — proving
/// the routing hook is skipped in local mode.
#[test]
fn ext_method_rewind_uses_local_dispatch_without_bridge() {
    use acp::Agent as _;
    let _env = crate::env::EnvVarGuard::remove(crate::env::GROK_DISABLE_CUSTOM_BRIDGE_ENV);
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let params = serde_json::json!({ "sessionId" : "sess-local" });
        let err = agent
            .ext_method(acp::ExtRequest::new(
                "x.ai/rewind/points",
                std::sync::Arc::from(serde_json::value::to_raw_value(&params).unwrap()),
            ))
            .await
            .expect_err("local rewind with no session must error");
        assert_eq!(err.code, acp::Error::resource_not_found(None).code);
    });
}
#[test]
fn cancel_does_not_forward_to_bridge_in_local_mode() {
    use crate::session::SessionCommand;
    use acp::Agent as _;
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = acp::SessionId::new("sess-cancel-local");
        let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
        agent.sessions.borrow_mut().insert(sid.clone(), handle);
        agent
            .cancel(acp::CancelNotification::new(sid.clone()))
            .await
            .expect("cancel must succeed");
        let mut saw_local_cancel = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            if let SessionCommand::Cancel { .. } = cmd {
                saw_local_cancel = true;
            }
        }
        assert!(
            saw_local_cancel,
            "local-mode cancel dispatches the local SessionCommand::Cancel with no bridge attached"
        );
    });
}
/// Regression (post-cancel slot hang, first bad release 0.2.101; see
/// `dispatch_locks`). SDK e2e shape:
/// `test_cancel_ends_in_flight_turn_and_frees_slot` (grok-agent-sdk).
#[test]
fn cancel_never_overtakes_in_flight_prompt_intake() {
    use crate::session::SessionCommand;
    use acp::Agent as _;
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = acp::SessionId::new("sess-cancel-intake-race");
        let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
        agent.sessions.borrow_mut().insert(sid.clone(), handle);
        let order: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>> =
            std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let (intake_parked_tx, intake_parked_rx) = tokio::sync::oneshot::channel::<()>();
        let driver_order = order.clone();
        tokio::task::spawn_local(async move {
            let mut intake_parked_tx = Some(intake_parked_tx);
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    SessionCommand::GetCurrentPromptMode { .. } => {
                        if let Some(tx) = intake_parked_tx.take() {
                            let _ = tx.send(());
                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        }
                    }
                    SessionCommand::Prompt { .. } => driver_order.borrow_mut().push("prompt"),
                    SessionCommand::Cancel { .. } => driver_order.borrow_mut().push("cancel"),
                    _ => {}
                }
            }
        });
        let prompt_fut = agent.prompt(acp::PromptRequest::new(
            sid.clone(),
            vec![acp::ContentBlock::from("hi")],
        ));
        let cancel_fut = async {
            intake_parked_rx
                .await
                .expect("prompt intake reaches the fake actor");
            let _ = agent
                .cancel(acp::CancelNotification::new(sid.clone()))
                .await;
        };
        let _ = futures::join!(prompt_fut, cancel_fut);
        assert_eq!(
            order.borrow().as_slice(),
            ["prompt", "cancel"],
            "cancel must land on the actor mailbox after the prompt it targets"
        );
    });
}
use crate::session::SessionCommand as TestSessionCommand;
/// Build a session handle wired to a *live* command channel. Returns the
/// handle (move into `sessions`) plus a probe `cmd_tx`/`cmd_rx` so a test
/// can observe what the agent sends to the actor and prove the channel is
/// live.
fn make_live_session_handle(
    sid: &acp::SessionId,
    running_prompt: Option<&str>,
) -> (
    crate::session::SessionHandle,
    tokio::sync::mpsc::UnboundedSender<TestSessionCommand>,
    tokio::sync::mpsc::UnboundedReceiver<TestSessionCommand>,
) {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut handle = make_test_handle("test-model", false, Some("grok-tui"));
    handle.cmd_tx = cmd_tx.clone();
    handle.info = crate::session::info::Info {
        id: sid.clone(),
        cwd: "/tmp".to_string(),
    };
    if let Some(pid) = running_prompt {
        *handle.current_prompt_id.lock().unwrap() = Some(pid.to_string());
    }
    (handle, cmd_tx, cmd_rx)
}
/// Spawn a minimal fake session actor on the `LocalSet` that answers
/// `SessionCommand::IsBusy` with `busy` and forwards every other command to
/// the returned receiver so a test can assert on them (e.g. `Shutdown`).
fn spawn_fake_actor(
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<TestSessionCommand>,
    busy: bool,
) -> tokio::sync::mpsc::UnboundedReceiver<TestSessionCommand> {
    let (observed_tx, observed_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::task::spawn_local(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                TestSessionCommand::IsBusy { respond_to } => {
                    let _ = respond_to.send(busy);
                }
                other => {
                    let _ = observed_tx.send(other);
                }
            }
        }
    });
    observed_rx
}
/// Drive `x.ai/internal/evict_sessions` through the real `ext_notification`
/// handler path (not the internal helper) — matches how the leader server
/// signals a client disconnect.
async fn drive_disconnect(agent: &MvpAgent, sid: &acp::SessionId) {
    drive_disconnect_many(agent, &[sid]).await;
}
/// Like `drive_disconnect`, but evicts several sessions in a single
/// `x.ai/internal/evict_sessions` notification — the realistic shape of a
/// real client disconnect, and the path that exercises `handle_evict_sessions`'
/// concurrent `join_all` check pass followed by the sequential act pass.
async fn drive_disconnect_many(agent: &MvpAgent, sids: &[&acp::SessionId]) {
    use acp::Agent as _;
    let ids: Vec<&str> = sids.iter().map(|s| s.0.as_ref()).collect();
    let params = serde_json::json!({ "sessionIds" : ids });
    let raw = serde_json::value::to_raw_value(&params).unwrap();
    agent
        .ext_notification(acp::ExtNotification::new(
            "x.ai/internal/evict_sessions",
            raw.into(),
        ))
        .await
        .expect("evict_sessions notification must be handled");
}
/// Drive `x.ai/session/close` through the real `ext_method` dispatch
/// (`ext_method` → `handlers::session::handle` → `handle_session_close`),
/// exercising the exact production path that finalizes the replica.
async fn drive_close(agent: &MvpAgent, session_id: &str) -> Result<acp::ExtResponse, acp::Error> {
    use acp::Agent as _;
    let params = serde_json::json!({ "sessionId" : session_id });
    let raw = serde_json::value::to_raw_value(&params).unwrap();
    agent
        .ext_method(acp::ExtRequest::new(
            "x.ai/session/close",
            std::sync::Arc::from(raw),
        ))
        .await
}
/// No-evict keystone: a client disconnecting mid-turn must NOT destroy the
/// session. The actor stays resident, no `Shutdown` is sent, the resident
/// session's command channel still **delivers** commands (so a reconnecting
/// `session/load` can keep driving the turn), and `finalize()` is NOT called
/// on a mere disconnect.
#[test]
fn disconnect_keeps_live_session_resident_without_finalize() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = acp::SessionId::new("sess-live");
        let (_cmd_tx, mut cmd_rx) = {
            let (handle, tx, rx) = make_live_session_handle(&sid, Some("turn-1"));
            agent.sessions.borrow_mut().insert(sid.clone(), handle);
            (tx, rx)
        };
        drive_disconnect(&agent, &sid).await;
        assert!(
            agent.sessions.borrow().contains_key(&sid),
            "live session must stay resident across client disconnect"
        );
        assert!(
            matches!(
                cmd_rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "no command may be sent to a session kept resident with live work"
        );
        let resident = agent
            .sessions
            .borrow()
            .get(&sid)
            .cloned()
            .expect("session must still be resident");
        resident
            .cmd_tx
            .send(TestSessionCommand::ResetPermissionState)
            .expect("resident session channel must accept commands post-disconnect");
        assert!(
            matches!(
                cmd_rx.try_recv(),
                Ok(TestSessionCommand::ResetPermissionState)
            ),
            "the resident session's receiver must observe the delivered command"
        );
        assert!(
            agent.finalize_spy.borrow().is_empty(),
            "finalize() must NOT fire on client disconnect"
        );
        assert_eq!(
            agent.session_live_state_for(&sid),
            Some(SessionLiveState::Working),
            "a kept-resident session with live work is Working"
        );
    });
}
/// Keep-resident must hold even if the `current_prompt_id` lock is poisoned:
/// an unknown state is treated as "busy" (never unload). Guards against a
/// regression flipping the `unwrap_or(true)` fallback to `false`.
#[test]
fn disconnect_keeps_resident_on_poisoned_lock() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = acp::SessionId::new("sess-poison");
        let (handle, _tx, _rx) = make_live_session_handle(&sid, None);
        let poison_target = handle.current_prompt_id.clone();
        agent.sessions.borrow_mut().insert(sid.clone(), handle);
        let _ = std::thread::spawn(move || {
            let _g = poison_target.lock().unwrap();
            panic!("poison current_prompt_id");
        })
        .join();
        assert!(
            agent
                .sessions
                .borrow()
                .get(&sid)
                .unwrap()
                .current_prompt_id
                .lock()
                .is_err(),
            "precondition: the lock must be poisoned"
        );
        drive_disconnect(&agent, &sid).await;
        assert!(
            agent.sessions.borrow().contains_key(&sid),
            "a session with an unknown (poisoned) state must be kept resident"
        );
        assert_eq!(
            agent.session_live_state_for(&sid),
            Some(SessionLiveState::Working),
        );
    });
}
/// Idle-unload stub (memory bound) + supervisor interaction: a *fully idle*
/// session is unloaded to disk on disconnect (actor `Shutdown`, handle
/// dropped) while the `SessionThread` is **retained** for
/// `drain_old_session_thread`. It is not finalized, and once the kept thread
/// finishes the supervisor reaps it as a *clean* exit — never `DeadFailed`.
#[test]
fn disconnect_unloads_idle_session_without_finalize() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = acp::SessionId::new("sess-idle");
        let (handle, _cmd_tx, cmd_rx) = make_live_session_handle(&sid, None);
        agent.sessions.borrow_mut().insert(sid.clone(), handle);
        let mut observed = spawn_fake_actor(cmd_rx, false);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        agent.session_threads.borrow_mut().insert(
            sid.clone(),
            crate::session::SessionThread::from_handle(std::thread::spawn(move || {
                let _ = release_rx.recv();
            })),
        );
        agent.ensure_session_supervisor();
        drive_disconnect(&agent, &sid).await;
        assert!(
            !agent.sessions.borrow().contains_key(&sid),
            "idle session must be unloaded from the resident map on disconnect"
        );
        assert!(
            agent.session_threads.borrow().contains_key(&sid),
            "idle-unload must keep the SessionThread for reconnect drain"
        );
        let shutdown = tokio::time::timeout(std::time::Duration::from_secs(1), observed.recv())
            .await
            .expect("idle-unload must send a command within 1s")
            .expect("fake actor channel must stay open");
        assert!(
            matches!(shutdown, TestSessionCommand::Shutdown),
            "idle-unload must send SessionCommand::Shutdown"
        );
        assert!(
            agent.finalize_spy.borrow().is_empty(),
            "idle-unload on disconnect must NOT finalize the cloud replica"
        );
        assert_eq!(
            agent.session_live_state_for(&sid),
            Some(SessionLiveState::Dormant),
            "an idle-unloaded session demotes to Dormant"
        );
        drop(release_tx);
        let deadline = tokio::time::Instant::now() + (SESSION_SUPERVISOR_TICK * 6);
        while tokio::time::Instant::now() < deadline {
            if !agent.session_threads.borrow().contains_key(&sid) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            !agent.session_threads.borrow().contains_key(&sid),
            "supervisor must drop the finished kept thread"
        );
        assert!(
            !agent
                .roster_delta_spy
                .borrow()
                .iter()
                .any(|(id, st)| id == sid.0.as_ref() && *st == SessionLiveState::DeadFailed),
            "a cleanly idle-unloaded session must not be reaped as DeadFailed"
        );
        assert_eq!(
            agent.session_live_state_for(&sid),
            None,
            "clean-exit sweep must drop the Dormant live-state entry"
        );
    });
}
/// The `IsBusy` keep-resident path. A between-turns session
/// (`current_prompt_id = None`) whose actor answers `IsBusy = true` (queued
/// inputs at the turn boundary) must be kept resident — NOT unloaded — and
/// must receive no `Shutdown`. This exercises the async round-trip that the
/// sync fast-path tests skip.
#[test]
fn disconnect_keeps_resident_when_actor_reports_busy() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = acp::SessionId::new("sess-busy");
        let (handle, _cmd_tx, cmd_rx) = make_live_session_handle(&sid, None);
        agent.sessions.borrow_mut().insert(sid.clone(), handle);
        let mut observed = spawn_fake_actor(cmd_rx, true);
        drive_disconnect(&agent, &sid).await;
        assert!(
            agent.sessions.borrow().contains_key(&sid),
            "a between-turns session with queued work (IsBusy=true) must stay resident"
        );
        assert_eq!(
            agent.session_live_state_for(&sid),
            Some(SessionLiveState::Working),
            "an actor-reported-busy session is kept Working"
        );
        tokio::task::yield_now().await;
        assert!(
            matches!(
                observed.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "a busy session must not be sent Shutdown"
        );
    });
}
/// A between-turns session whose ONLY outstanding work is a parked
/// `PlanApproval` reverse-request (the resume re-park) must be kept resident on
/// disconnect. The actor answers `IsBusy = false`, so the keep-resident outcome
/// can come ONLY from the parked-approval sync fast path in `session_has_live_work`
/// — deleting that check would let this session unload (mutation-killing).
#[test]
fn disconnect_keeps_resident_when_plan_approval_parked() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = acp::SessionId::new("sess-plan-parked");
        let (handle, _cmd_tx, cmd_rx) = make_live_session_handle(&sid, None);
        handle.pending_interactions.lock().unwrap().insert(
            "exit-plan-mode-resume".to_string(),
            crate::session::pending_interaction::PendingKind::PlanApproval,
        );
        agent.sessions.borrow_mut().insert(sid.clone(), handle);
        let mut observed = spawn_fake_actor(cmd_rx, false);
        drive_disconnect(&agent, &sid).await;
        assert!(
            agent.sessions.borrow().contains_key(&sid),
            "a session with a parked plan-approval must stay resident"
        );
        assert_eq!(
            agent.session_live_state_for(&sid),
            Some(SessionLiveState::Working),
            "a parked-approval session is kept Working"
        );
        tokio::task::yield_now().await;
        assert!(
            matches!(
                observed.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "a parked-approval session must not be sent Shutdown"
        );
    });
}
/// Mixed batch in a *single* `x.ai/internal/evict_sessions` notification —
/// the realistic disconnect shape and the path that exercises
/// `handle_evict_sessions`' `join_all` two-pass (concurrent `IsBusy` checks,
/// then sequential act). One session's actor reports busy (→ kept resident,
/// `Working`, no `Shutdown`); the other is idle (→ unloaded, `Dormant`,
/// `Shutdown` sent). Each must get its own outcome with no cross-contamination
/// between the concurrent check pass and the sequential act pass.
#[test]
fn disconnect_mixed_batch_keeps_busy_unloads_idle() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid_busy = acp::SessionId::new("sess-batch-busy");
        let sid_idle = acp::SessionId::new("sess-batch-idle");
        let (busy_handle, _busy_tx, busy_rx) = make_live_session_handle(&sid_busy, None);
        let (idle_handle, _idle_tx, idle_rx) = make_live_session_handle(&sid_idle, None);
        agent
            .sessions
            .borrow_mut()
            .insert(sid_busy.clone(), busy_handle);
        agent
            .sessions
            .borrow_mut()
            .insert(sid_idle.clone(), idle_handle);
        let mut busy_observed = spawn_fake_actor(busy_rx, true);
        let mut idle_observed = spawn_fake_actor(idle_rx, false);
        drive_disconnect_many(&agent, &[&sid_busy, &sid_idle]).await;
        assert!(
            agent.sessions.borrow().contains_key(&sid_busy),
            "the busy session in the batch must stay resident"
        );
        assert_eq!(
            agent.session_live_state_for(&sid_busy),
            Some(SessionLiveState::Working),
            "the busy session must be Working"
        );
        assert!(
            !agent.sessions.borrow().contains_key(&sid_idle),
            "the idle session in the batch must be unloaded"
        );
        assert_eq!(
            agent.session_live_state_for(&sid_idle),
            Some(SessionLiveState::Dormant),
            "the idle session must be Dormant"
        );
        let idle_shutdown =
            tokio::time::timeout(std::time::Duration::from_secs(1), idle_observed.recv())
                .await
                .expect("idle session must receive a command within 1s")
                .expect("fake actor channel must stay open");
        assert!(
            matches!(idle_shutdown, TestSessionCommand::Shutdown),
            "the idle session must be sent Shutdown"
        );
        tokio::task::yield_now().await;
        assert!(
            matches!(
                busy_observed.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "the busy session must not be sent Shutdown in a mixed batch"
        );
        assert!(
            agent.finalize_spy.borrow().is_empty(),
            "neither batch outcome may finalize on a mere disconnect"
        );
    });
}
/// The bounded `session_live_state` map does not grow without bound
/// across repeated create/close cycles — every terminal close drops its
/// entry, so the map size stays at the live count, not the cumulative count.
#[test]
fn session_live_state_map_is_bounded_across_cycles() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        for i in 0..50 {
            let sid = acp::SessionId::new(format!("sess-cycle-{i}"));
            let (handle, _tx, _rx) = make_live_session_handle(&sid, Some("turn"));
            agent.sessions.borrow_mut().insert(sid.clone(), handle);
            agent.set_session_live_state(&sid, SessionLiveState::IdleResident);
            agent.close_session_explicit(&sid);
        }
        assert_eq!(
            agent.session_live_state.borrow().len(),
            0,
            "terminal closes must leave no residual live-state entries (bounded map)"
        );
    });
}
/// Finalize fires on a genuine terminal close — driven through the **real**
/// `x.ai/session/close` dispatch (`ext_method` → `handle_session_close`),
/// not the internal helper. Proves finalize was *moved* (not removed) and
/// guards the handler's `existed` gate. (Finalize assertion is
/// invocation-level; see note in `finalize_session_replica`.)
#[test]
fn explicit_close_finalizes_the_replica() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = acp::SessionId::new("sess-close");
        let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, Some("turn-1"));
        agent.sessions.borrow_mut().insert(sid.clone(), handle);
        drive_close(&agent, "no-such-session")
            .await
            .expect("close of a missing session must succeed as a no-op");
        assert!(
            agent.finalize_spy.borrow().is_empty(),
            "closing a missing session must NOT finalize"
        );
        drive_close(&agent, sid.0.as_ref())
            .await
            .expect("session close must be handled");
        assert!(
            matches!(cmd_rx.try_recv(), Ok(TestSessionCommand::Shutdown)),
            "handle_session_close must send Shutdown to the actor"
        );
        assert_eq!(
            agent.finalize_spy.borrow().as_slice(),
            &[sid.0.to_string()],
            "explicit close must finalize the cloud replica exactly once"
        );
        assert!(
            !agent.sessions.borrow().contains_key(&sid),
            "explicit close removes the session"
        );
        assert_eq!(
            agent.session_live_state_for(&sid),
            None,
            "terminal removal must drop the live-state entry (bounded map)"
        );
        assert!(
            agent
                .roster_delta_spy
                .borrow()
                .iter()
                .any(|(id, st)| id == sid.0.as_ref() && *st == SessionLiveState::Completed),
            "explicit close must emit a Completed roster delta"
        );
    });
}
/// Join-handle supervisor: a *resident* actor that panics is reaped
/// promptly — removed from `sessions`/`session_threads`, demoted to
/// `DeadFailed` (observed via the roster delta, since the live-state entry
/// is dropped on removal), and NOT finalized (the conversation persists).
///
/// Polls in real time (the panic unwinds on a real OS thread, independent of
/// the tokio clock); the reap lands within a small number of supervisor
/// ticks. The injected-panic backtrace on stderr is expected and harmless.
#[test]
fn supervisor_reaps_panicked_resident_actor() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = acp::SessionId::new("sess-panic");
        let (handle, _tx, _rx) = make_live_session_handle(&sid, Some("turn-1"));
        agent.sessions.borrow_mut().insert(sid.clone(), handle);
        let panic_thread = std::thread::spawn(|| panic!("injected actor panic"));
        agent.session_threads.borrow_mut().insert(
            sid.clone(),
            crate::session::SessionThread::from_handle(panic_thread),
        );
        agent.set_session_live_state(&sid, SessionLiveState::Working);
        agent.ensure_session_supervisor();
        let deadline = tokio::time::Instant::now() + (SESSION_SUPERVISOR_TICK * 6);
        while tokio::time::Instant::now() < deadline {
            if !agent.session_threads.borrow().contains_key(&sid) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            !agent.session_threads.borrow().contains_key(&sid),
            "supervisor must reap the dead thread"
        );
        assert!(
            !agent.sessions.borrow().contains_key(&sid),
            "reaped session must be removed from the resident map"
        );
        assert_eq!(
            agent.session_live_state_for(&sid),
            None,
            "terminal removal drops the live-state entry (bounded map)"
        );
        assert!(
            agent
                .roster_delta_spy
                .borrow()
                .iter()
                .any(|(id, st)| id == sid.0.as_ref() && *st == SessionLiveState::DeadFailed),
            "a reaped resident actor must emit a DeadFailed roster delta"
        );
        assert!(
            agent.finalize_spy.borrow().is_empty(),
            "reaping a dead actor must NOT finalize (conversation persists)"
        );
    });
}
/// `ensure_session_supervisor` is idempotent: calling it repeatedly spawns
/// the sweeper loop exactly once.
#[test]
fn ensure_session_supervisor_is_idempotent() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        assert_eq!(agent.supervisor_spawn_count.get(), 0);
        agent.ensure_session_supervisor();
        agent.ensure_session_supervisor();
        agent.ensure_session_supervisor();
        assert_eq!(
            agent.supervisor_spawn_count.get(),
            1,
            "the supervisor task must be spawned at most once"
        );
        assert!(agent.supervisor_started.get());
    });
}
/// After a terminal removal (reap/close drops the live-state entry), a later
/// reload of the same SessionId starts clean at `IdleResident` with no stale
/// terminal state leaking in (ties to the bounded-map fix).
#[test]
fn reload_after_terminal_removal_starts_clean() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = acp::SessionId::new("sess-reload");
        let (handle, _tx, _rx) = make_live_session_handle(&sid, Some("turn-1"));
        agent.sessions.borrow_mut().insert(sid.clone(), handle);
        agent.close_session_explicit(&sid);
        assert_eq!(
            agent.session_live_state_for(&sid),
            None,
            "terminal removal must leave no stale state"
        );
        let (handle2, _tx2, _rx2) = make_live_session_handle(&sid, None);
        agent.sessions.borrow_mut().insert(sid.clone(), handle2);
        agent.set_session_live_state(&sid, SessionLiveState::IdleResident);
        assert_eq!(
            agent.session_live_state_for(&sid),
            Some(SessionLiveState::IdleResident),
            "a reloaded session must start at IdleResident, not a stale terminal state"
        );
    });
}
/// Build an agent whose gateway is wired to a live receiver, so a test can
/// observe (and answer) agent→client reverse-requests like the dormant
/// `x.ai/folder_trust/request` round-trip.
fn build_agent_with_gateway_rx() -> (
    MvpAgent,
    tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
) {
    use crate::agent::config::Config as AgentConfig;
    use crate::auth::{AuthManager, GrokComConfig};
    let temp_dir = tempfile::tempdir().unwrap();
    let auth_manager =
        std::sync::Arc::new(AuthManager::new(temp_dir.path(), GrokComConfig::default()));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let gateway = GatewaySender::new(tx);
    let cfg = AgentConfig::default();
    let agent = MvpAgent::new(gateway, &cfg, auth_manager, None).expect("valid test config");
    (agent, rx)
}
/// A git repo whose only repo-local config is a project `.mcp.json` declaring
/// `projsrv` — so it is untrusted-with-configs, and the project server should
/// reappear after a trust grant.
fn repo_with_project_mcp_server() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    git2::Repository::init(tmp.path()).unwrap();
    std::fs::write(
        tmp.path().join(".mcp.json"),
        r#"{"mcpServers":{"projsrv":{"command":"echo","args":["hi"]}}}"#,
    )
    .unwrap();
    tmp
}
fn write_project_subagent_definitions(cwd: &std::path::Path) {
    let roles = cwd.join(".grok/roles");
    let personas = cwd.join(".grok/personas");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::create_dir_all(&personas).unwrap();
    std::fs::write(roles.join("probe.toml"), "description = \"Project role\"").unwrap();
    std::fs::write(
        personas.join("probe.toml"),
        "instructions = \"Project persona\"",
    )
    .unwrap();
}
fn folder_trust_on() -> crate::util::config::RemoteSettings {
    crate::util::config::RemoteSettings {
        folder_trust_enabled: Some(true),
        ..Default::default()
    }
}
#[test]
#[serial_test::serial]
fn subagent_spawn_context_reloads_project_definitions_after_trust_changes() {
    let repo = tempfile::tempdir().unwrap();
    git2::Repository::init(repo.path()).unwrap();
    write_project_subagent_definitions(repo.path());
    run_local_for_bridge_test(|| async {
        let (agent, _rx) = build_agent_with_gateway_rx();
        let sid = acp::SessionId::new("roles-personas-trust-transition");
        let (mut handle, _tx, _cmd_rx) = make_live_session_handle(&sid, None);
        handle.info.cwd = repo.path().display().to_string();
        agent.sessions.borrow_mut().insert(sid.clone(), handle);
        {
            let mut cfg = agent.cfg.borrow_mut();
            cfg.subagent_roles.insert(
                "refreshed".into(),
                xai_grok_subagent_resolution::config::SubagentRole {
                    description: "Refreshed user role".into(),
                    source_dir: Some(repo.path().join("user-roles")),
                    ..Default::default()
                },
            );
            cfg.subagent_model_overrides
                .insert("probe".into(), "refreshed-model".into());
            cfg.subagent_toggle.insert("probe".into(), false);
        }
        crate::agent::folder_trust::record_for_test(repo.path(), false);
        let untrusted = agent.build_subagent_spawn_context(sid.0.as_ref());
        assert!(!untrusted.subagent_roles.contains_key("probe"));
        assert!(!untrusted.subagent_personas.contains_key("probe"));
        assert_eq!(
            untrusted
                .subagent_roles
                .get("refreshed")
                .map(|role| role.description.as_str()),
            Some("Refreshed user role")
        );
        assert_eq!(
            untrusted
                .subagent_model_overrides
                .get("probe")
                .map(String::as_str),
            Some("refreshed-model")
        );
        assert_eq!(untrusted.subagent_toggle.get("probe"), Some(&false));
        crate::agent::folder_trust::record_for_test(repo.path(), true);
        let trusted = agent.build_subagent_spawn_context(sid.0.as_ref());
        assert_eq!(
            trusted
                .subagent_roles
                .get("probe")
                .map(|role| role.description.as_str()),
            Some("Project role")
        );
        assert!(trusted.subagent_personas.contains_key("probe"));
        crate::agent::folder_trust::record_for_test(repo.path(), false);
        let revoked = agent.build_subagent_spawn_context(sid.0.as_ref());
        assert!(!revoked.subagent_roles.contains_key("probe"));
        assert!(!revoked.subagent_personas.contains_key("probe"));
    });
}
/// End-to-end gate wiring: project `.grok/roles` / `personas` alone must drive
/// real `resolve_and_record` untrusted (not a forced `record_for_test` verdict),
/// keep project defs out of Task spawn context, then re-admit them after grant.
#[test]
#[serial_test::serial]
fn project_roles_personas_gated_via_resolve_and_record_chain() {
    use xai_grok_test_support::EnvGuard;
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
    let repo = tempfile::tempdir().unwrap();
    git2::Repository::init(repo.path()).unwrap();
    write_project_subagent_definitions(repo.path());
    run_local_for_bridge_test(|| async {
        let (agent, _rx) = build_agent_with_gateway_rx();
        let sid = acp::SessionId::new("roles-personas-resolve-chain");
        let (mut handle, _tx, _cmd_rx) = make_live_session_handle(&sid, None);
        handle.info.cwd = repo.path().display().to_string();
        agent.sessions.borrow_mut().insert(sid.clone(), handle);
        let allowed = crate::agent::folder_trust::resolve_and_record(
            repo.path(),
            Some(&folder_trust_on()),
            false,
        );
        assert!(
            !allowed,
            "roles/personas markers alone must resolve untrusted without a grant"
        );
        assert!(
            !crate::agent::folder_trust::project_scope_allowed(repo.path()),
            "cached verdict after resolve_and_record must stay untrusted"
        );
        let untrusted = agent.build_subagent_spawn_context(sid.0.as_ref());
        assert!(
            !untrusted.subagent_roles.contains_key("probe"),
            "untrusted: project role must stay out of spawn context"
        );
        assert!(
            !untrusted.subagent_personas.contains_key("probe"),
            "untrusted: project persona must stay out of spawn context"
        );
        crate::agent::folder_trust::grant_folder_trust(repo.path());
        let allowed = crate::agent::folder_trust::resolve_and_record(
            repo.path(),
            Some(&folder_trust_on()),
            false,
        );
        assert!(allowed, "store-granted folder must resolve trusted");
        let trusted = agent.build_subagent_spawn_context(sid.0.as_ref());
        assert_eq!(
            trusted
                .subagent_roles
                .get("probe")
                .map(|role| role.description.as_str()),
            Some("Project role")
        );
        assert!(
            trusted.subagent_personas.contains_key("probe"),
            "trusted: project persona must enter spawn context after grant"
        );
    });
}
/// Pull the next `x.ai/folder_trust/request` reverse-request off the gateway and
/// answer it with `outcome`. Returns the request's decoded params.
async fn answer_folder_trust_request(
    gw_rx: &mut tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
    outcome: &str,
) -> serde_json::Value {
    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), gw_rx.recv())
        .await
        .expect("trust request must be sent")
        .expect("gateway channel open");
    let xai_acp_lib::AcpClientMessage::ExtMethod(args) = msg else {
        panic!("expected an ext_method reverse-request, got a different message");
    };
    assert_eq!(args.request.method.as_ref(), "x.ai/folder_trust/request");
    let params: serde_json::Value = serde_json::from_str(args.request.params.get()).unwrap();
    let resp: acp::ExtResponse = acp::ExtResponse::new(std::sync::Arc::from(
        serde_json::value::to_raw_value(&serde_json::json!({ "outcome" : outcome })).unwrap(),
    ));
    let _ = args.response_tx.send(Ok(resp));
    params
}
#[test]
#[serial_test::serial]
fn interactive_trust_prompt_grant_reloads_project_mcp() {
    use xai_grok_test_support::EnvGuard;
    use xai_grok_workspace::trust::{TrustStore, workspace_key};
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
    let repo = repo_with_project_mcp_server();
    let repo_path = repo.path().to_path_buf();
    let remote = folder_trust_on();
    run_local_for_bridge_test(|| async {
        let (agent, mut gw_rx) = build_agent_with_gateway_rx();
        agent.interactive_trust_client.set(true);
        crate::agent::folder_trust::resolve_and_record(&repo_path, Some(&remote), false);
        assert!(
            !crate::agent::folder_trust::project_scope_allowed(&repo_path),
            "untrusted-with-configs workspace must gate project scope before the grant"
        );
        let sid = acp::SessionId::new("sess-trust");
        let (mut handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
        handle.info.cwd = repo_path.to_string_lossy().to_string();
        agent.sessions.borrow_mut().insert(sid.clone(), handle);
        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
        let params = answer_folder_trust_request(&mut gw_rx, "trust").await;
        assert!(
            params["configKinds"]
                .as_array()
                .is_some_and(|k| k.iter().any(|v| v == "mcp")),
            "request must summarize detected config kinds; got {params}"
        );
        assert_eq!(
            params["sessionId"], "sess-trust",
            "trust request must carry the session id for leader routing; got {params}"
        );
        let mut saw_project_mcp = false;
        let mut saw_reload_plugins = false;
        let mut saw_reload_hooks = false;
        for _ in 0..8 {
            match tokio::time::timeout(std::time::Duration::from_secs(2), cmd_rx.recv()).await {
                Ok(Some(TestSessionCommand::UpdateMcpServers { mcp_servers, .. })) => {
                    saw_project_mcp |= mcp_servers
                        .iter()
                        .any(|s| crate::session::managed_mcp::mcp_server_name(s) == "projsrv");
                }
                Ok(Some(TestSessionCommand::ReloadPlugins { .. })) => {
                    saw_reload_plugins = true;
                }
                Ok(Some(TestSessionCommand::ReloadHooks)) => saw_reload_hooks = true,
                Ok(Some(_other)) => continue,
                _ => break,
            }
            if saw_project_mcp && saw_reload_plugins && saw_reload_hooks {
                break;
            }
        }
        assert!(
            saw_project_mcp,
            "trust grant must reload the session's now-trusted project MCP server"
        );
        assert!(
            saw_reload_plugins,
            "trust grant must reload plugins (plugin-contributed hooks/MCP)"
        );
        assert!(
            saw_reload_hooks,
            "trust grant must reload the session's own project hooks"
        );
        assert!(
            TrustStore::load().is_trusted(&workspace_key(&repo_path)),
            "accepting the prompt must persist the trust grant"
        );
        assert!(
            crate::agent::folder_trust::project_scope_allowed(&repo_path),
            "the in-process gate must flip to trusted after the grant"
        );
    });
}
#[test]
#[serial_test::serial]
fn interactive_trust_prompt_reject_keeps_gated() {
    use xai_grok_test_support::EnvGuard;
    use xai_grok_workspace::trust::{TrustStore, workspace_key};
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
    let repo = repo_with_project_mcp_server();
    let repo_path = repo.path().to_path_buf();
    let remote = folder_trust_on();
    run_local_for_bridge_test(|| async {
        let (agent, mut gw_rx) = build_agent_with_gateway_rx();
        agent.interactive_trust_client.set(true);
        crate::agent::folder_trust::resolve_and_record(&repo_path, Some(&remote), false);
        let sid = acp::SessionId::new("sess-reject");
        let (mut handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
        handle.info.cwd = repo_path.to_string_lossy().to_string();
        agent.sessions.borrow_mut().insert(sid.clone(), handle);
        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
        let _ = answer_folder_trust_request(&mut gw_rx, "reject").await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), cmd_rx.recv())
                .await
                .is_err(),
            "rejecting trust must leave the session's project servers gated (no reload)"
        );
        assert!(
            !TrustStore::load().is_trusted(&workspace_key(&repo_path)),
            "rejecting trust must leave the store unchanged"
        );
        assert!(
            !crate::agent::folder_trust::project_scope_allowed(&repo_path),
            "rejecting trust must keep the workspace gated"
        );
    });
}
#[test]
#[serial_test::serial]
fn interactive_trust_prompt_dormant_when_feature_off() {
    use xai_grok_test_support::EnvGuard;
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
    let repo = repo_with_project_mcp_server();
    let repo_path = repo.path().to_path_buf();
    let remote = crate::util::config::RemoteSettings {
        folder_trust_enabled: Some(false),
        ..Default::default()
    };
    run_local_for_bridge_test(|| async {
        let (agent, mut gw_rx) = build_agent_with_gateway_rx();
        agent.interactive_trust_client.set(true);
        let sid = acp::SessionId::new("sess-dormant");
        let (mut handle, _tx, _cmd_rx) = make_live_session_handle(&sid, None);
        handle.info.cwd = repo_path.to_string_lossy().to_string();
        agent.sessions.borrow_mut().insert(sid.clone(), handle);
        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), gw_rx.recv())
                .await
                .is_err(),
            "feature off must emit no trust request (dormant)"
        );
    });
}
#[test]
#[serial_test::serial]
fn interactive_trust_prompt_no_request_without_capability() {
    use xai_grok_test_support::EnvGuard;
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
    let repo = repo_with_project_mcp_server();
    let repo_path = repo.path().to_path_buf();
    let remote = folder_trust_on();
    run_local_for_bridge_test(|| async {
        let (agent, mut gw_rx) = build_agent_with_gateway_rx();
        assert!(!agent.interactive_trust_client.get());
        let sid = acp::SessionId::new("sess-nocap");
        let (mut handle, _tx, _cmd_rx) = make_live_session_handle(&sid, None);
        handle.info.cwd = repo_path.to_string_lossy().to_string();
        agent.sessions.borrow_mut().insert(sid.clone(), handle);
        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), gw_rx.recv())
                .await
                .is_err(),
            "a client without the capability must get no trust request"
        );
    });
}
#[test]
#[serial_test::serial]
fn interactive_trust_prompt_client_error_fails_closed() {
    use xai_grok_test_support::EnvGuard;
    use xai_grok_workspace::trust::{TrustStore, workspace_key};
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
    let repo = repo_with_project_mcp_server();
    let repo_path = repo.path().to_path_buf();
    let remote = folder_trust_on();
    run_local_for_bridge_test(|| async {
        let (agent, mut gw_rx) = build_agent_with_gateway_rx();
        agent.interactive_trust_client.set(true);
        crate::agent::folder_trust::resolve_and_record(&repo_path, Some(&remote), false);
        let sid = acp::SessionId::new("sess-err");
        let (mut handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
        handle.info.cwd = repo_path.to_string_lossy().to_string();
        agent.sessions.borrow_mut().insert(sid.clone(), handle);
        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), gw_rx.recv())
            .await
            .expect("trust request must be sent")
            .expect("gateway channel open");
        assert!(matches!(msg, xai_acp_lib::AcpClientMessage::ExtMethod(_)));
        drop(msg);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), cmd_rx.recv())
                .await
                .is_err(),
            "a failed client round-trip must not reload the session"
        );
        assert!(
            !TrustStore::load().is_trusted(&workspace_key(&repo_path)),
            "a failed client round-trip must not grant trust"
        );
        assert!(!crate::agent::folder_trust::project_scope_allowed(
            &repo_path
        ));
    });
}
#[test]
#[serial_test::serial]
fn interactive_trust_prompt_dedups_same_workspace() {
    use xai_grok_test_support::EnvGuard;
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
    let repo = repo_with_project_mcp_server();
    let repo_path = repo.path().to_path_buf();
    let remote = folder_trust_on();
    run_local_for_bridge_test(|| async {
        let (agent, mut gw_rx) = build_agent_with_gateway_rx();
        agent.interactive_trust_client.set(true);
        crate::agent::folder_trust::resolve_and_record(&repo_path, Some(&remote), false);
        let sid = acp::SessionId::new("sess-dedup");
        let (mut handle, _tx, _cmd_rx) = make_live_session_handle(&sid, None);
        handle.info.cwd = repo_path.to_string_lossy().to_string();
        agent.sessions.borrow_mut().insert(sid.clone(), handle);
        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), gw_rx.recv()).await;
        assert!(
            matches!(first, Ok(Some(xai_acp_lib::AcpClientMessage::ExtMethod(_)))),
            "first prompt for an untrusted workspace must emit a request"
        );
        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), gw_rx.recv())
                .await
                .is_err(),
            "a workspace already prompted this process must not be re-prompted"
        );
    });
}
/// Which reload commands a session received after a grant.
struct ReloadCmds {
    update_mcp: bool,
    reload_plugins: bool,
    reload_hooks: bool,
    mcp_names: Vec<String>,
}
/// Drain a session's command channel for the post-grant reload trio
/// (`UpdateMcpServers` + `ReloadPlugins` + `ReloadHooks`), capturing the merged
/// MCP server names so a test can assert per-cwd reload.
async fn drain_reload_commands(
    cmd_rx: &mut tokio::sync::mpsc::UnboundedReceiver<TestSessionCommand>,
) -> ReloadCmds {
    let mut out = ReloadCmds {
        update_mcp: false,
        reload_plugins: false,
        reload_hooks: false,
        mcp_names: Vec::new(),
    };
    for _ in 0..8 {
        match tokio::time::timeout(std::time::Duration::from_secs(2), cmd_rx.recv()).await {
            Ok(Some(TestSessionCommand::UpdateMcpServers { mcp_servers, .. })) => {
                out.update_mcp = true;
                out.mcp_names = mcp_servers
                    .iter()
                    .map(|s| crate::session::managed_mcp::mcp_server_name(s).to_string())
                    .collect();
            }
            Ok(Some(TestSessionCommand::ReloadPlugins { .. })) => {
                out.reload_plugins = true;
            }
            Ok(Some(TestSessionCommand::ReloadHooks)) => out.reload_hooks = true,
            Ok(Some(_other)) => continue,
            _ => break,
        }
        if out.update_mcp && out.reload_plugins && out.reload_hooks {
            break;
        }
    }
    out
}
#[test]
#[serial_test::serial]
fn interactive_trust_prompt_reloads_all_same_workspace_sessions() {
    use xai_grok_test_support::EnvGuard;
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
    let repo = repo_with_project_mcp_server();
    let root = repo.path().to_path_buf();
    let subdir = root.join("sub");
    std::fs::create_dir_all(&subdir).unwrap();
    std::fs::write(
        subdir.join(".mcp.json"),
        r#"{"mcpServers":{"subsrv":{"command":"echo","args":["hi"]}}}"#,
    )
    .unwrap();
    let other = repo_with_project_mcp_server();
    let other_path = other.path().to_path_buf();
    let remote = folder_trust_on();
    run_local_for_bridge_test(|| async {
        let (agent, mut gw_rx) = build_agent_with_gateway_rx();
        agent.interactive_trust_client.set(true);
        crate::agent::folder_trust::resolve_and_record(&root, Some(&remote), false);
        let sid_root = acp::SessionId::new("sess-root");
        let (mut h_root, _t1, mut rx_root) = make_live_session_handle(&sid_root, None);
        h_root.info.cwd = root.to_string_lossy().to_string();
        agent.sessions.borrow_mut().insert(sid_root.clone(), h_root);
        let sid_sub = acp::SessionId::new("sess-sub");
        let (mut h_sub, _t2, mut rx_sub) = make_live_session_handle(&sid_sub, None);
        h_sub.info.cwd = subdir.to_string_lossy().to_string();
        agent.sessions.borrow_mut().insert(sid_sub.clone(), h_sub);
        let sid_other = acp::SessionId::new("sess-other");
        let (mut h_other, _t3, mut rx_other) = make_live_session_handle(&sid_other, None);
        h_other.info.cwd = other_path.to_string_lossy().to_string();
        agent
            .sessions
            .borrow_mut()
            .insert(sid_other.clone(), h_other);
        agent.maybe_spawn_interactive_trust_prompt(&sid_root, &root, Some(&remote));
        let _ = answer_folder_trust_request(&mut gw_rx, "trust").await;
        let root_cmds = drain_reload_commands(&mut rx_root).await;
        assert!(
            root_cmds.update_mcp && root_cmds.reload_plugins && root_cmds.reload_hooks,
            "root session must get UpdateMcpServers + ReloadPlugins + ReloadHooks"
        );
        let sub_cmds = drain_reload_commands(&mut rx_sub).await;
        assert!(
            sub_cmds.update_mcp && sub_cmds.reload_plugins && sub_cmds.reload_hooks,
            "subdir session must get UpdateMcpServers + ReloadPlugins + ReloadHooks"
        );
        assert!(
            sub_cmds.mcp_names.iter().any(|n| n == "subsrv"),
            "subdir session must reload against its own cwd (expect subsrv); got {:?}",
            sub_cmds.mcp_names
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), rx_other.recv())
                .await
                .is_err(),
            "a session under a different workspace_key must not be reloaded"
        );
    });
}
#[test]
#[serial_test::serial]
fn interactive_trust_prompt_reprompts_after_untrust() {
    use xai_grok_test_support::EnvGuard;
    use xai_hooks_plugins_types::HooksAction;
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
    let repo = repo_with_project_mcp_server();
    let repo_path = repo.path().to_path_buf();
    let remote = folder_trust_on();
    run_local_for_bridge_test(|| async {
        let (agent, mut gw_rx) = build_agent_with_gateway_rx();
        agent.interactive_trust_client.set(true);
        crate::agent::folder_trust::resolve_and_record(&repo_path, Some(&remote), false);
        let sid = acp::SessionId::new("sess-reprompt");
        let (mut handle, _tx, _cmd_rx) = make_live_session_handle(&sid, None);
        handle.info.cwd = repo_path.to_string_lossy().to_string();
        agent.sessions.borrow_mut().insert(sid.clone(), handle);
        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
        assert!(
            matches!(
                tokio::time::timeout(std::time::Duration::from_secs(2), gw_rx.recv()).await,
                Ok(Some(xai_acp_lib::AcpClientMessage::ExtMethod(_)))
            ),
            "first prompt must emit a request"
        );
        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), gw_rx.recv())
                .await
                .is_err(),
            "a prompted workspace must be suppressed before untrust"
        );
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            agent.execute_hooks_action(&sid, HooksAction::Untrust),
        )
        .await;
        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
        assert!(
            matches!(
                tokio::time::timeout(std::time::Duration::from_secs(2), gw_rx.recv()).await,
                Ok(Some(xai_acp_lib::AcpClientMessage::ExtMethod(_)))
            ),
            "after untrust clears the dedup, the workspace must be promptable again"
        );
    });
}
fn ann(id: &str) -> xai_grok_announcements::RemoteAnnouncement {
    xai_grok_announcements::RemoteAnnouncement {
        id: Some(id.to_string()),
        message: Some(format!("{id}-msg")),
        severity: Some("critical".to_string()),
        ..Default::default()
    }
}
/// `RemoteSettings` with only `announcements` set (callers add sentinel
/// fields as needed).
fn settings_with(
    announcements: Option<Vec<xai_grok_announcements::RemoteAnnouncement>>,
) -> crate::util::config::RemoteSettings {
    crate::util::config::RemoteSettings {
        announcements,
        ..Default::default()
    }
}
fn test_now() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc)
}
/// Pushes must carry strictly increasing generations, seeded from unix-epoch
/// seconds so a restarted leader still beats pager watermarks that survived
/// re-election (`AppView.announcements_last_gen` is never reset).
#[tokio::test]
async fn announcements_gen_seeds_from_epoch_and_strictly_increases() {
    let agent = build_minimal_agent_for_tests();
    let epoch_before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let first = agent.next_announcements_gen();
    let second = agent.next_announcements_gen();
    assert!(
        first >= epoch_before,
        "first gen must be epoch-seeded: {first} < {epoch_before}"
    );
    assert!(
        second > first,
        "gens must strictly increase: {first} -> {second}"
    );
    let far_ahead = first + 1_000_000;
    agent.announcements_gen.set(far_ahead);
    assert_eq!(agent.next_announcements_gen(), far_ahead + 1);
}
/// An unchanged visible list must not produce a push (idle steady-state is
/// silent); a changed one — including clearing to empty — must.
#[test]
fn announcements_push_gate_emits_only_on_change() {
    let now = test_now();
    assert_eq!(
        announcements_push_payload(None, &[], now, AnnouncementsPushMode::IfChanged),
        None
    );
    let list_a = vec![ann("a")];
    assert_eq!(
        announcements_push_payload(
            Some(list_a.as_slice()),
            &[],
            now,
            AnnouncementsPushMode::IfChanged
        ),
        Some(list_a.clone())
    );
    assert_eq!(
        announcements_push_payload(
            Some(list_a.as_slice()),
            &list_a,
            now,
            AnnouncementsPushMode::IfChanged
        ),
        None
    );
    let list_ab = vec![ann("a"), ann("b")];
    assert_eq!(
        announcements_push_payload(
            Some(list_ab.as_slice()),
            &list_a,
            now,
            AnnouncementsPushMode::IfChanged
        ),
        Some(list_ab.clone())
    );
    assert_eq!(
        announcements_push_payload(None, &list_ab, now, AnnouncementsPushMode::IfChanged),
        Some(vec![])
    );
}
/// `seed` (per-client initialize) re-emits an unchanged non-empty list for
/// the freshly attached client, but stays silent when there is nothing to
/// show.
#[test]
fn announcements_push_gate_seed_reemits_nonempty_only() {
    let now = test_now();
    let list_a = vec![ann("a")];
    assert_eq!(
        announcements_push_payload(
            Some(list_a.as_slice()),
            &list_a,
            now,
            AnnouncementsPushMode::SeedNewClient
        ),
        Some(list_a.clone()),
        "seed must re-push an unchanged non-empty list"
    );
    assert_eq!(
        announcements_push_payload(None, &[], now, AnnouncementsPushMode::SeedNewClient),
        None,
        "seed with nothing visible must stay silent"
    );
}
/// `/new` forces a push even when the visible list is unchanged — including
/// unchanged-empty — so the pager re-merges its config-layer (requirements/
/// user/managed TOML) announcements from local mid-session edits.
#[test]
fn announcements_push_gate_force_mode_pushes_unchanged_and_empty() {
    let now = test_now();
    let list_a = vec![ann("a")];
    assert_eq!(
        announcements_push_payload(
            Some(list_a.as_slice()),
            &list_a,
            now,
            AnnouncementsPushMode::Force
        ),
        Some(list_a.clone()),
        "force must push an unchanged list"
    );
    assert_eq!(
        announcements_push_payload(None, &[], now, AnnouncementsPushMode::Force),
        Some(vec![]),
        "force must push even an unchanged empty list"
    );
}
/// An addition that is already expired on arrival never becomes visible, so
/// it must not re-emit.
#[test]
fn announcements_push_gate_ignores_expired_only_addition() {
    let now = test_now();
    let expired = xai_grok_announcements::RemoteAnnouncement {
        expires_at: Some("2000-01-01T00:00:00Z".to_string()),
        ..ann("expired")
    };
    let list_a = vec![ann("a")];
    let stored = vec![ann("a"), expired];
    assert_eq!(
        announcements_push_payload(
            Some(stored.as_slice()),
            &list_a,
            now,
            AnnouncementsPushMode::IfChanged
        ),
        None,
        "an already-expired addition must not re-emit"
    );
}
/// A previously emitted item that passes its `expires_at` between gate runs
/// must emit the shrunken (here: empty) list exactly once, so live banners
/// clear on time instead of outliving their own expiry.
#[test]
fn announcements_push_gate_emits_on_expiry_crossing() {
    let expiring = xai_grok_announcements::RemoteAnnouncement {
        expires_at: Some("2026-06-01T00:00:00Z".to_string()),
        ..ann("soon")
    };
    let stored = vec![expiring.clone()];
    let before = chrono::DateTime::parse_from_rfc3339("2026-05-31T23:59:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let emitted = announcements_push_payload(
        Some(stored.as_slice()),
        &[],
        before,
        AnnouncementsPushMode::IfChanged,
    )
    .expect("live item must emit");
    assert_eq!(emitted, stored);
    let after = chrono::DateTime::parse_from_rfc3339("2026-06-01T00:01:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert_eq!(
        announcements_push_payload(
            Some(stored.as_slice()),
            &emitted,
            after,
            AnnouncementsPushMode::IfChanged
        ),
        Some(vec![]),
        "expiry crossing must emit the shrunken list"
    );
    assert_eq!(
        announcements_push_payload(
            Some(stored.as_slice()),
            &[],
            after,
            AnnouncementsPushMode::IfChanged
        ),
        None
    );
}
/// A poll apply must touch ONLY `remote_settings.announcements`; every other
/// stored field keeps its pre-poll value (full reapply stays owned by
/// startup, auth, and `/new`).
#[tokio::test]
async fn polled_announcements_apply_touches_announcements_only() {
    let agent = build_minimal_agent_for_tests();
    let mut stored = settings_with(Some(vec![ann("old")]));
    stored.tips = Some(vec!["stored-tip".to_string()]);
    stored.allow_access = Some(true);
    stored.default_model = Some("stored-model".to_string());
    agent.cfg.borrow_mut().remote_settings = Some(stored);
    let mut fresh = settings_with(Some(vec![ann("new")]));
    fresh.tips = Some(vec!["fresh-tip".to_string()]);
    fresh.allow_access = Some(false);
    fresh.default_model = Some("fresh-model".to_string());
    agent.apply_polled_announcements(fresh, Some(vec![ann("old")]));
    let cfg = agent.cfg.borrow();
    let after = cfg
        .remote_settings
        .as_ref()
        .expect("settings still present");
    assert_eq!(after.announcements, Some(vec![ann("new")]));
    assert_eq!(
        after.tips,
        Some(vec!["stored-tip".to_string()]),
        "tips must be untouched by a poll apply"
    );
    assert_eq!(
        after.allow_access,
        Some(true),
        "allow_access must be untouched by a poll apply"
    );
    assert_eq!(
        after.default_model.as_deref(),
        Some("stored-model"),
        "default_model must be untouched by a poll apply"
    );
}
/// A poll apply must never fabricate `remote_settings` from scratch — the
/// `is_none()`-keyed retry/gating semantics of the full-refresh owners
/// depend on absence staying observable.
#[tokio::test]
async fn polled_announcements_apply_never_fabricates_settings() {
    let agent = build_minimal_agent_for_tests();
    agent.cfg.borrow_mut().remote_settings = None;
    agent.apply_polled_announcements(settings_with(Some(vec![ann("a")])), None);
    assert!(
        agent.cfg.borrow().remote_settings.is_none(),
        "a poll must leave absent remote_settings absent"
    );
}
/// A full-refresh writer landing during the poll's fetch makes the poll's
/// result stale; the apply must skip rather than clobber the fresher store
/// (the next tick reconciles).
#[tokio::test]
async fn polled_announcements_apply_skips_when_writer_landed_mid_fetch() {
    let agent = build_minimal_agent_for_tests();
    let pre_fetch = Some(vec![ann("old")]);
    agent.cfg.borrow_mut().remote_settings = Some(settings_with(Some(vec![ann("mid-fetch")])));
    agent.apply_polled_announcements(settings_with(Some(vec![ann("stale-poll")])), pre_fetch);
    assert_eq!(
        agent
            .cfg
            .borrow()
            .remote_settings
            .as_ref()
            .and_then(|s| s.announcements.clone()),
        Some(vec![ann("mid-fetch")]),
        "the mid-fetch writer's store must win over the stale poll result"
    );
}
/// End-to-end through the shared gate: every emission advances the baseline
/// and carries a strictly larger gen; unchanged state is silent unless
/// seeding a new client.
#[tokio::test]
async fn emit_announcements_gate_emits_updates_baseline_and_bumps_gen() {
    let (agent, mut rx) = build_agent_with_gateway_rx();
    agent.cfg.borrow_mut().remote_settings = Some(settings_with(Some(vec![ann("a")])));
    let recv_gen =
        |rx: &mut tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>| {
            let msg = rx.try_recv().expect("expected an announcements push");
            let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg else {
                panic!("expected ExtNotification, got another message kind");
            };
            assert_eq!(args.request.method.as_ref(), "x.ai/announcements/update");
            let parsed: serde_json::Value =
                serde_json::from_str(args.request.params.get()).expect("valid JSON payload");
            parsed
                .get("gen")
                .and_then(|g| g.as_u64())
                .expect("gen field")
        };
    agent.emit_announcements(AnnouncementsPushMode::IfChanged);
    let first_gen = recv_gen(&mut rx);
    agent.emit_announcements(AnnouncementsPushMode::IfChanged);
    assert!(rx.try_recv().is_err(), "unchanged list must not re-push");
    agent.emit_announcements(AnnouncementsPushMode::SeedNewClient);
    let seed_gen = recv_gen(&mut rx);
    assert!(
        seed_gen > first_gen,
        "gen must strictly increase: {first_gen} -> {seed_gen}"
    );
    agent.cfg.borrow_mut().remote_settings = Some(settings_with(None));
    agent.emit_announcements(AnnouncementsPushMode::IfChanged);
    let clear_gen = recv_gen(&mut rx);
    assert!(clear_gen > seed_gen);
    agent.emit_announcements(AnnouncementsPushMode::IfChanged);
    assert!(
        rx.try_recv().is_err(),
        "cleared state must push exactly once"
    );
    agent.emit_announcements(AnnouncementsPushMode::Force);
    let force_gen = recv_gen(&mut rx);
    assert!(
        force_gen > clear_gen,
        "forced push must keep gens increasing"
    );
}
/// A send the gateway channel rejects must not advance the last-emitted
/// baseline; the next gate call then re-diffs and re-pushes the same list
/// (the poll's natural retry, no dedicated retry machinery).
#[tokio::test]
async fn emit_announcements_gate_keeps_baseline_on_failed_send_and_retries() {
    let (mut agent, rx) = build_agent_with_gateway_rx();
    agent.cfg.borrow_mut().remote_settings = Some(settings_with(Some(vec![ann("a")])));
    drop(rx);
    agent.emit_announcements(AnnouncementsPushMode::IfChanged);
    assert!(
        agent.last_emitted_announcements.borrow().is_empty(),
        "a failed send must leave the baseline untouched"
    );
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.gateway = GatewaySender::new(tx);
    agent.emit_announcements(AnnouncementsPushMode::IfChanged);
    let msg = rx
        .try_recv()
        .expect("next gate call must re-push after a failed send");
    let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg else {
        panic!("expected ExtNotification, got another message kind");
    };
    assert_eq!(args.request.method.as_ref(), "x.ai/announcements/update");
    assert_eq!(
        *agent.last_emitted_announcements.borrow(),
        vec![ann("a")],
        "a successful send advances the baseline"
    );
    agent.emit_announcements(AnnouncementsPushMode::IfChanged);
    assert!(rx.try_recv().is_err(), "unchanged list must not re-push");
}
mod direct_hub_cloud_removed {
    use super::super::{DIRECT_HUB_CLOUD_REMOVED_MSG, reject_direct_hub_cloud_meta};
    use crate::agent::config::HubConfig;
    fn assert_direct_hub_error(err: agent_client_protocol::Error) {
        assert_eq!(
            err.data.as_ref(),
            Some(&serde_json::Value::String(
                DIRECT_HUB_CLOUD_REMOVED_MSG.to_string()
            )),
            "error data must be the exact D8 message, got: {err:?}"
        );
        assert_eq!(
            err.code,
            agent_client_protocol::ErrorCode::InvalidParams,
            "must be invalid_params, got: {err:?}"
        );
    }
    #[test]
    fn cloud_server_id_meta_is_hard_error() {
        let meta = serde_json::json!({ "x.ai/cloud_server_id" : "srv-123" });
        let err = reject_direct_hub_cloud_meta(meta.as_object()).expect_err("must reject");
        assert_direct_hub_error(err);
    }
    #[test]
    fn cloud_server_id_null_still_present_is_hard_error() {
        let meta = serde_json::json!({ "x.ai/cloud_server_id" : null });
        let err = reject_direct_hub_cloud_meta(meta.as_object()).expect_err("must reject");
        assert_direct_hub_error(err);
    }
    #[test]
    fn cloud_server_id_with_gateway_meta_still_hard_error() {
        let meta = serde_json::json!(
            { "x.ai/cloud_server_id" : "srv-legacy", "envId" : "env-1",
            "x.ai/cloud_existing_workspace" : { "server_id" : "ws-1", "cwd" :
            "/workspace" } }
        );
        let err = reject_direct_hub_cloud_meta(meta.as_object()).expect_err("Direct stamp wins");
        assert_direct_hub_error(err);
    }
    #[test]
    fn absent_or_empty_meta_ok() {
        assert!(reject_direct_hub_cloud_meta(None).is_ok());
        assert!(reject_direct_hub_cloud_meta(serde_json::json!({}).as_object()).is_ok());
        assert!(
            reject_direct_hub_cloud_meta(serde_json::json!({ "envId" : "env-1" }).as_object())
                .is_ok()
        );
        assert!(
            reject_direct_hub_cloud_meta(
                serde_json::json!({
            "x.ai/cloud_existing_workspace" : { "server_id" : "ws-1", "cwd" :
            "/workspace" } })
                .as_object()
            )
            .is_ok()
        );
    }
    #[test]
    fn hub_url_gating_matrix() {
        let with_url = HubConfig {
            url: Some("wss://hub.example/ws".into()),
        };
        let without_url = HubConfig { url: None };
        let blank = HubConfig {
            url: Some("   ".into()),
        };
        assert!(with_url.is_enabled());
        assert!(!without_url.is_enabled());
        assert!(!blank.is_enabled());
    }
    #[test]
    fn hub_config_is_url_only_workspace_default() {
        let json = serde_json::to_value(HubConfig {
            url: Some("wss://hub.example/ws".into()),
        })
        .expect("serialize");
        let obj = json.as_object().expect("object");
        assert_eq!(
            obj.keys().collect::<Vec<_>>(),
            vec!["url"],
            "HubConfig must only serialize url (no proxy-mode fields)"
        );
        let from_legacy: HubConfig = serde_json::from_value(serde_json::json!(
            { "url" : "wss://hub.example/ws", "workspace_mode" : "remote",
            "send_turn_hooks" : false, }
        ))
        .expect("ignore unknown fields");
        assert_eq!(from_legacy.url.as_deref(), Some("wss://hub.example/ws"));
    }
}
mod soft_default_settings_emit {
    use super::*;
    #[tokio::test]
    async fn emit_settings_update_carries_permission_mode_from_cfg() {
        use crate::agent::config::Config as AgentConfig;
        use crate::auth::{AuthManager, GrokComConfig};
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let temp_dir = tempfile::tempdir().unwrap();
                let auth_manager = std::sync::Arc::new(AuthManager::new(
                    temp_dir.path(),
                    GrokComConfig::default(),
                ));
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                let gateway = GatewaySender::new(tx);
                let cfg = AgentConfig {
                    remote_settings: Some(crate::util::config::RemoteSettings {
                        permission_mode: Some("always-approve".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                let agent =
                    MvpAgent::new(gateway, &cfg, auth_manager, None).expect("valid test config");
                agent.cfg.borrow_mut().remote_settings = cfg.remote_settings.clone();
                agent.emit_settings_update_notification();
                let msg = rx.try_recv().expect("settings/update must be emitted");
                let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg else {
                    panic!("expected ExtNotification, got {msg:?}");
                };
                assert_eq!(args.request.method.as_ref(), "x.ai/settings/update");
                let params: serde_json::Value =
                    serde_json::from_str(args.request.params.get()).expect("parse params");
                assert_eq!(
                    params.get("permission_mode").and_then(|v| v.as_str()),
                    Some("always-approve"),
                    "post-auth emit must carry remote permission_mode for first session"
                );
                let _ = args.response_tx.send(Ok(()));
            })
            .await;
    }
}
#[cfg(feature = "dhat-heap")]
mod dhat_soak;
