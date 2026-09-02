use super::{AgentConfig, PREFETCH_RUNS, StartupPrefetch, apply_post_gate_settings};

#[test]
#[serial_test::serial(remote_sig_disarm)]
fn post_gate_pass_spends_at_most_one_settings_budget() {
    let runs_before = PREFETCH_RUNS.with(std::cell::Cell::get);

    let mut cfg = AgentConfig::default();
    assert!(
        cfg.remote_settings.is_none(),
        "an absent prefetch result is the state under test"
    );
    apply_post_gate_settings(&mut cfg, StartupPrefetch::ClientSupplied);
    assert_eq!(
        PREFETCH_RUNS.with(std::cell::Cell::get),
        runs_before + 1,
        "the fallback prefetch never ran: the counter is dead or the wiring lost the fetch"
    );

    let mut cfg = AgentConfig::default();
    apply_post_gate_settings(&mut cfg, StartupPrefetch::Ran);
    assert_eq!(
        PREFETCH_RUNS.with(std::cell::Cell::get),
        runs_before + 1,
        "the post-gate pass spent a second settings retry budget"
    );
}

#[test]
#[serial_test::serial(remote_sig_disarm)]
fn supplied_settings_consume_the_pending_fetch() {
    crate::agent::models::startup_prefetch::inject_for_tests(None);
    let mut cfg = AgentConfig {
        remote_settings: Some(Default::default()),
        ..AgentConfig::default()
    };
    let _ = super::ensure_remote_settings_side_effects(&mut cfg);
    assert!(
        !crate::agent::models::startup_prefetch::inflight_for_tests(),
        "a supplied-settings pass must consume the pending fetch, not strand it"
    );
}
