//! Its own binary: the grok home resolves once per process.

mod common;

use xai_grok_shell::agent::models::startup_prefetch;

#[test]
fn prefetch_never_starts_while_policy_repair_is_pending() {
    let home = common::isolated_home();
    common::block_on(async {
        let server = common::start_seeded_mock(home.path()).await;
        // A team principal with no serving managed policy: `ensure_managed_policy_present`
        // will run a session-start repair, so no prefetch may egress before it.
        let scope = xai_grok_shell::auth::GrokComConfig::default().auth_scope();
        let auth = serde_json::json!({
            scope: {
                "key": "team-session-token",
                "auth_mode": "oidc",
                "oidc_issuer": xai_grok_shell::auth::xai_oauth2_issuer(),
                "create_time": "2026-01-01T00:00:00Z",
                "expires_at": "2099-01-01T00:00:00Z",
                "user_id": "test-user",
                "principal_type": "Team",
                "team_id": "team-1",
            }
        });
        std::fs::write(home.path().join("auth.json"), auth.to_string())
            .expect("write team auth.json");

        startup_prefetch::begin_before_policy_gate(
            &xai_grok_shell::agent::config::Config::default(),
        );

        assert!(
            !startup_prefetch::inflight_for_tests(),
            "a pending policy repair must suppress the startup prefetch"
        );
        assert_eq!(
            (
                server.request_count_for("/v1/models"),
                server.request_count_for("/v1/settings"),
            ),
            (0, 0),
            "no authenticated request may leave before the policy repair"
        );
    });
}
