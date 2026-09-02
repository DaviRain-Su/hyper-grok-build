//! Its own binary: the grok home resolves once per process.

mod common;

use xai_grok_shell::agent::models::startup_prefetch;
use xai_grok_shell::util::config::RemoteSettings;

#[test]
fn bootstrap_discards_prefetch_when_repair_disables_remote_fetch() {
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
        std::fs::write(
            home.path().join("requirements.toml"),
            "[features]\nremote_fetch = false\n",
        )
        .expect("write requirements.toml");
        let resolved = common::run_bootstrap()
            .await
            .expect("bootstrap succeeds for a personal profile");

        assert!(
            resolved.remote_settings.is_none(),
            "a prefetch started before the policy repair must not be applied once remote_fetch is disabled"
        );
        assert_eq!(
            (
                server.request_count_for("/v1/models"),
                server.request_count_for("/v1/settings"),
            ),
            (0, 0),
            "the serial fallback must honor the repaired policy"
        );
    });
}
