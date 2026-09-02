use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::{
    Accept, INFLIGHT, Inflight, State, accept, accept_with_deadline, begin_before_policy_gate,
    clear_for_tests, inject_with_origin_for_tests,
};
use crate::agent::config::Config;
use crate::util::config::RemoteSettings;

fn marker_settings() -> Option<RemoteSettings> {
    Some(RemoteSettings {
        path_not_found_hints: Some(true),
        ..RemoteSettings::default()
    })
}

fn registered_marker() -> Option<bool> {
    let inflight = INFLIGHT.lock().unwrap();
    let cell = inflight.as_ref()?;
    let state = cell.state.lock().unwrap();
    state.settings.as_ref().and_then(|s| s.path_not_found_hints)
}

/// Same serial group as `init_tests`: both consume the process-wide fetch.
#[test]
#[serial_test::serial(remote_sig_disarm)]
fn begin_does_not_replace_an_inflight_fetch() {
    clear_for_tests();
    super::inject_for_tests(marker_settings());
    begin_before_policy_gate(&Config::default());
    assert_eq!(
        registered_marker(),
        Some(true),
        "the second begin must join the in-flight fetch, not replace it"
    );
    clear_for_tests();
}

#[test]
#[serial_test::serial(remote_sig_disarm)]
fn accept_discards_a_fetch_from_another_origin() {
    clear_for_tests();
    inject_with_origin_for_tests(marker_settings(), "https://elsewhere.invalid".to_string());
    assert!(
        matches!(accept(), Accept::Miss),
        "a fetch from a different origin must not be applied"
    );
    assert!(
        !super::inflight_for_tests(),
        "the rejected fetch must be consumed, not left registered"
    );
}

#[test]
#[serial_test::serial(remote_sig_disarm)]
fn accept_deadline_spends_the_budget() {
    clear_for_tests();
    let never_finishing = Arc::new(Inflight {
        origin: super::resolve_startup_endpoints().proxy_url(),
        state: Mutex::new(State::default()),
        done: Condvar::new(),
    });
    *INFLIGHT.lock().unwrap() = Some(never_finishing);
    assert!(
        matches!(accept_with_deadline(Duration::ZERO), Accept::Consumed(None)),
        "a timed-out wait must spend the budget, not trigger a refetch"
    );
    assert!(
        super::inflight_for_tests(),
        "a timed-out fetch must stay registered so nothing can start behind it"
    );
    clear_for_tests();
}

#[test]
#[serial_test::serial(remote_sig_disarm)]
fn wait_settings_leaves_the_fetch_for_accept() {
    if !crate::util::config::resolve_remote_fetch_enabled() {
        eprintln!("skipped: remote_fetch disabled in this environment");
        return;
    }
    clear_for_tests();
    super::inject_for_tests(marker_settings());
    assert_eq!(
        super::wait_settings(Duration::ZERO).and_then(|s| s.path_not_found_hints),
        Some(true),
    );
    match accept() {
        Accept::Consumed(settings) => assert_eq!(
            settings.and_then(|s| s.path_not_found_hints),
            Some(true),
            "wait_settings must not consume the fetch"
        ),
        Accept::Miss => panic!("wait_settings consumed the fetch"),
    }
}
