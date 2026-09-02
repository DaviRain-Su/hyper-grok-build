//! Its own binary: the grok home resolves once per process.

mod common;

use xai_grok_shell::agent::models::startup_prefetch;
use xai_grok_shell::util::config::RemoteSettings;

#[test]
fn bootstrap_consumes_supplied_prefetch_without_refetching() {
    let home = common::isolated_home();
    common::block_on(async {
        let server = common::start_seeded_mock(home.path()).await;
        server.set_settings(RemoteSettings {
            tips: Some(vec!["not-the-marker".into()]),
            ..RemoteSettings::default()
        });

        startup_prefetch::inject_for_tests(Some(RemoteSettings {
            path_not_found_hints: Some(true),
            ..RemoteSettings::default()
        }));
        let resolved = common::run_bootstrap()
            .await
            .expect("bootstrap succeeds for a personal profile");

        assert_eq!(
            (
                server.request_count_for("/v1/models"),
                server.request_count_for("/v1/settings"),
            ),
            (0, 0),
            "bootstrap must consume the supplied prefetch instead of fetching"
        );
        assert_eq!(
            resolved
                .remote_settings
                .as_ref()
                .and_then(|s| s.path_not_found_hints),
            Some(true),
            "the supplied prefetch's settings must reach the resolved config"
        );
    });
}
