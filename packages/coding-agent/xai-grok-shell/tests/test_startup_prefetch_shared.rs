//! Its own binary: the grok home resolves once per process.

mod common;

use xai_grok_shell::agent::models::startup_prefetch;
use xai_grok_shell::util::config::RemoteSettings;

#[test]
fn one_fetch_serves_begin_wait_and_bootstrap() {
    let home = common::isolated_home();
    common::block_on(async {
        let server = common::start_seeded_mock(home.path()).await;
        server.set_settings(RemoteSettings {
            tips: Some(vec!["from-server".into()]),
            ..RemoteSettings::default()
        });

        assert!(startup_prefetch::begin(None), "the first begin must start");
        assert!(
            startup_prefetch::begin(None),
            "the second begin must join, not restart"
        );
        let waited = startup_prefetch::wait_settings(std::time::Duration::from_secs(10));
        assert_eq!(
            waited.and_then(|s| s.tips),
            Some(vec!["from-server".to_string()]),
            "the shared fetch must serve the early wait"
        );

        let resolved = common::run_bootstrap()
            .await
            .expect("bootstrap succeeds for a personal profile");

        assert_eq!(
            (
                server.request_count_for("/v1/models"),
                server.request_count_for("/v1/settings"),
            ),
            (1, 1),
            "begin, wait, and bootstrap together must spend one settings budget"
        );
        assert_eq!(
            resolved
                .remote_settings
                .as_ref()
                .and_then(|s| s.tips.clone()),
            Some(vec!["from-server".to_string()]),
            "bootstrap must consume the same fetch the wait observed"
        );
    });
}
