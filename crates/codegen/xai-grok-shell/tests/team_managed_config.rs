//! End-to-end client tests for team-OAuth managed config against a mock
//! deployment-config endpoint. Proxy-side resolution is unit-tested in
//! the cli-chat-proxy deployment-config route.
//!
//! Every test here MUST be `#[serial]`: they share one process-global
//! `GROK_HOME` (the `grok_home` `OnceLock` allows a single value per process)
//! and mutate that directory + process env, so concurrent tests would race.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use serial_test::serial;
use xai_grok_shell::config::ServingIdentity;
use xai_grok_test_support::spawn_counting_server;

/// The serving identity for a team id (the staleness checks key on this).
fn team_identity(id: &str) -> ServingIdentity {
    ServingIdentity::Team(id.to_owned())
}

/// Shared temp dir used as GROK_HOME for the whole test binary (the grok_home
/// `OnceLock` only allows one value per process). Also scrubs/installs the env
/// this suite depends on, before any test thread reads it.
fn test_home() -> &'static PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let path = tempfile::TempDir::new().unwrap().keep();
        // SAFETY: set once at init before other threads read the vars.
        unsafe {
            std::env::set_var("GROK_HOME", &path);
            // Ambient env must not shadow the scenarios under test: a real
            // deployment key, a managed-config opt-out, or a proxy that would
            // intercept the 127.0.0.1 mocks.
            for var in [
                "GROK_DEPLOYMENT_KEY",
                "GROK_MANAGED_CONFIG",
                "GROK_DEPLOYMENT_CONFIG_REFRESH_INTERVAL_SECS",
                "GROK_DEPLOYMENT_CONFIG_CACHE_TTL_SECS",
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
                "http_proxy",
                "https_proxy",
                "all_proxy",
            ] {
                std::env::remove_var(var);
            }
            // Real exponential backoff would add seconds per retry test.
            std::env::set_var("GROK_DEPLOYMENT_CONFIG_BACKOFF_MS", "10");
        }
        path
    })
}

fn reset(home: &std::path::Path) {
    for f in [
        "config.toml",
        "auth.json",
        "managed_config.toml",
        "requirements.toml",
        "managed_config.sig.json",
        "managed_config_cache.json",
        "managed_config.lock",
    ] {
        let _ = std::fs::remove_file(home.join(f));
    }
}

/// Read one HTTP request's header block (up to the blank line) and return the
/// `Authorization` header value, if any. Header-boundary-safe, unlike a single
/// fixed-size `read()`.
fn read_request_auth(stream: &mut std::net::TcpStream) -> Option<String> {
    let mut reader = BufReader::new(stream);
    let mut auth = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return auth;
        }
        let line = line.trim_end();
        if line.is_empty() {
            return auth;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("authorization")
        {
            auth = Some(value.trim().to_string());
        }
    }
}

/// Mock deployment-config server serving `body` to every request. Returns the
/// URL and the `Authorization` header of every request in order.
fn spawn_mock(body: String) -> (String, Arc<Mutex<Vec<String>>>) {
    let (url, _count, auths) = spawn_mock_seq(vec![(200, body)]);
    (url, auths)
}

/// Like [`spawn_mock_seq`] but sleeps `delay` before each response (for mid-fetch races).
fn spawn_mock_delayed(body: String, delay: std::time::Duration) -> MockHandle {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let counter = count.clone();
    let auths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_auths = auths.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            if let Some(auth) = read_request_auth(&mut stream) {
                seen_auths.lock().unwrap().push(auth);
            }
            {
                *counter.lock().unwrap() += 1;
            }
            std::thread::sleep(delay);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://{addr}/v1/deployment-config"), count, auths)
}

/// `(url, request_count, authorization_headers_in_order)`.
type MockHandle = (String, Arc<Mutex<usize>>, Arc<Mutex<Vec<String>>>);

/// Mock server that serves a sequence of `(status, body)` responses — response
/// `i` for request `i`, clamping to the last. The handle's request counter
/// backs retry/fail-fast assertions; the auth log backs credential-fallback
/// assertions.
fn spawn_mock_seq(responses: Vec<(u16, String)>) -> MockHandle {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let counter = count.clone();
    let auths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_auths = auths.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            if let Some(auth) = read_request_auth(&mut stream) {
                seen_auths.lock().unwrap().push(auth);
            }
            let i = {
                let mut c = counter.lock().unwrap();
                let idx = *c;
                *c += 1;
                idx
            };
            let (status, body) = responses
                .get(i)
                .or_else(|| responses.last())
                .cloned()
                .unwrap_or((200, "{}".to_string()));
            let reason = match status {
                200 => "OK",
                401 => "Unauthorized",
                500 => "Internal Server Error",
                _ => "Status",
            };
            let resp = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://{addr}/deployment/config"), count, auths)
}

/// Mock that abruptly closes its first `close_first` connections right after
/// reading the request — simulating a stale/poisoned keep-alive connection the
/// client reused — then serves `body` (HTTP 200) on every later connection. The
/// counter records accepted connections, backing the retry assertion.
fn spawn_mock_closing_first(close_first: usize, body: String) -> (String, Arc<Mutex<usize>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let counter = count.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            // Read the request first so the abort lands mid-response ("connection
            // closed before message completed"), not as a connect/write failure.
            let _ = read_request_auth(&mut stream);
            let i = {
                let mut c = counter.lock().unwrap();
                let idx = *c;
                *c += 1;
                idx
            };
            if i < close_first {
                drop(stream);
                continue;
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://{addr}/deployment/config"), count)
}

/// Mock that, for its first `truncate_first` connections, writes a valid status line
/// + headers with an OVERSIZED `Content-Length` then closes WITHOUT the body — so the
/// client's body read fails mid-body ("connection closed before message completed", a
/// `reqwest` body-phase error, NOT a decode error). Every later connection serves the
/// full valid `body` (HTTP 200). The counter records accepted connections, backing the
/// retry assertion.
fn spawn_mock_truncating_body_first(
    truncate_first: usize,
    body: String,
) -> (String, Arc<Mutex<usize>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let counter = count.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            // Read the request first so the abort lands in the body phase, not as a
            // connect/write failure.
            let _ = read_request_auth(&mut stream);
            let i = {
                let mut c = counter.lock().unwrap();
                let idx = *c;
                *c += 1;
                idx
            };
            if i < truncate_first {
                // Promise 100000 body bytes, send none, then drop: the client reads the
                // headers fine but fails collecting the body.
                let headers = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(headers.as_bytes());
                let _ = stream.flush();
                drop(stream);
                continue;
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://{addr}/deployment/config"), count)
}

/// Write a `config.toml` that routes the managed-config fetch at the mock.
fn write_config(home: &std::path::Path, managed_config_url: &str) {
    std::fs::write(
        home.join("config.toml"),
        format!("[endpoints]\nmanaged_config_url = \"{managed_config_url}\"\n"),
    )
    .unwrap();
}

/// Write an `auth.json` with a team OAuth principal under the active scope.
fn write_team_auth(home: &std::path::Path, team_id: &str) {
    write_team_auth_expiry(home, team_id, "2099-01-01T00:00:00Z");
}

/// Like [`write_team_auth`] but with an explicit `expires_at`, so tests can
/// simulate a routine cold-start where the persisted access token is expired.
fn write_team_auth_expiry(home: &std::path::Path, team_id: &str, expires_at: &str) {
    let scope = xai_grok_shell::auth::GrokComConfig::default().auth_scope();
    let auth = serde_json::json!({
        scope: {
            "key": "team-session-token",
            "auth_mode": "oidc",
            "create_time": "2026-01-01T00:00:00Z",
            "expires_at": expires_at,
            "user_id": "user-1",
            "principal_type": "Team",
            "team_id": team_id,
        }
    });
    std::fs::write(home.join("auth.json"), auth.to_string()).unwrap();
}

/// Write an `auth.json` with an EXPIRED `external`-mode team principal, so a configured refresher
/// drives `AuthManager::auth()`. Models the cold-start where the persisted token is expired but refreshable.
fn write_expired_external_team_auth(home: &std::path::Path, team_id: &str) {
    let scope = xai_grok_shell::auth::GrokComConfig::default().auth_scope();
    let auth = serde_json::json!({
        scope: {
            "key": "stale-team-token",
            "auth_mode": "external",
            "create_time": "2026-01-01T00:00:00Z",
            "expires_at": PAST,
            "user_id": "user-1",
            "principal_type": "Team",
            "team_id": team_id,
            "refresh_token": "rt-team",
        }
    });
    std::fs::write(home.join("auth.json"), auth.to_string()).unwrap();
}

const FAR_FUTURE: &str = "2099-01-01T00:00:00Z";
const PAST: &str = "2000-01-01T00:00:00Z";

const TEAM_MANAGED: &str = "[[marketplace.sources]]\nname = \"internal\"\ngit = \"https://github.com/example/plugin-marketplace-internal\"\n";
const TEAM_REQUIREMENTS: &str =
    "[marketplace]\nallowlist = [\"https://github.com/example/plugin-marketplace-internal\"]\n";

fn team_config_body() -> String {
    serde_json::json!({
        "deployment_id": serde_json::Value::Null,
        "team_id": "team-007",
        "managed_config": TEAM_MANAGED,
        "requirements": TEAM_REQUIREMENTS,
    })
    .to_string()
}

#[tokio::test]
#[serial]
async fn team_sync_writes_files() {
    let home = test_home().clone();
    reset(&home);

    let (url, auths) = spawn_mock(team_config_body());
    write_config(&home, &url);
    write_team_auth(&home, "team-007");

    let wrote = xai_grok_shell::managed_config::sync()
        .await
        .expect("sync should succeed");
    assert!(wrote, "expected team config to be written");

    assert_eq!(
        auths.lock().unwrap().last().map(String::as_str),
        Some("Bearer team-session-token"),
        "client must authenticate with the team session token"
    );

    let managed = std::fs::read_to_string(home.join("managed_config.toml")).unwrap();
    assert!(
        managed.contains("plugin-marketplace-internal"),
        "managed_config should contain the team marketplace source: {managed}"
    );

    let requirements = std::fs::read_to_string(home.join("requirements.toml")).unwrap();
    assert!(
        requirements.contains("allowlist"),
        "requirements should contain the enforced allowlist: {requirements}"
    );
}

/// A directory squatting at the MARKER path must not permanently disarm the staleness
/// detector: the atomic marker write would fail onto it on every sync, forever. The
/// locked apply clears the squat (same rule as the sidecar) and records the sync.
#[tokio::test]
#[serial]
async fn marker_dir_squat_is_cleared_and_marker_written() {
    let home = test_home().clone();
    reset(&home);

    let (url, _auths) = spawn_mock(team_config_body());
    write_config(&home, &url);
    write_team_auth(&home, "team-007");

    // Dir-squat the marker (with a child, like a real squat).
    let marker_path = home.join(xai_grok_config::MANAGED_CONFIG_CACHE_FILE);
    std::fs::create_dir(&marker_path).unwrap();
    std::fs::write(marker_path.join("junk"), "x").unwrap();

    let wrote = xai_grok_shell::managed_config::sync()
        .await
        .expect("sync should succeed");
    assert!(
        wrote,
        "the policy files are written despite the marker squat"
    );

    assert!(
        marker_path.is_file(),
        "the apply must replace the squatting directory with the marker FILE"
    );
    let marker = std::fs::read_to_string(&marker_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&marker).unwrap();
    assert_eq!(
        v["principal"].as_str(),
        Some("team-007"),
        "the recorded marker must describe this sync: {marker}"
    );
}

/// A whitespace-padded `team_id` in `auth.json` is one identity end-to-end: the serving
/// identity and the recorded marker are trimmed, and re-syncing with the padded id is the
/// same tenant (no eviction, no confirmed switch).
#[tokio::test]
#[serial]
async fn padded_team_id_is_one_identity() {
    let home = test_home().clone();
    reset(&home);

    let (url, _auths) = spawn_mock(team_config_body());
    write_config(&home, &url);
    write_team_auth(&home, "  team-007  ");

    assert_eq!(
        xai_grok_shell::managed_config::current_serving_identity(),
        team_identity("team-007"),
        "the serving identity must be the trimmed team id"
    );
    xai_grok_shell::managed_config::sync()
        .await
        .expect("sync should succeed");
    let marker =
        std::fs::read_to_string(home.join(xai_grok_config::MANAGED_CONFIG_CACHE_FILE)).unwrap();
    let v: serde_json::Value = serde_json::from_str(&marker).unwrap();
    assert_eq!(
        v["principal"].as_str(),
        Some("team-007"),
        "the marker stores the trimmed identity: {marker}"
    );
    assert_eq!(
        xai_grok_config::confirmed_team_switch("team-007"),
        None,
        "padding is not a tenant switch"
    );
}

/// Switching the active team must not keep enforcing the prior team's policy: after B syncs,
/// A's artifacts are evicted and the marker records B served nothing. Fail-open.
#[tokio::test]
#[serial]
async fn team_switch_evicts_prior_teams_policy() {
    let home = test_home().clone();
    reset(&home);

    // Team A serves both managed_config and requirements. A leftover sidecar (from an
    // earlier signing build; verification is inactive here) must also be evicted, or a
    // later signing build would read A's foreign-bound sidecar against B's identity.
    let (url_a, _auths_a) = spawn_mock(team_config_body());
    write_config(&home, &url_a);
    write_team_auth(&home, "team-a");
    xai_grok_shell::managed_config::sync()
        .await
        .expect("team A sync should succeed");
    assert!(home.join("requirements.toml").exists());
    assert!(home.join("managed_config.toml").exists());
    std::fs::write(home.join("managed_config.sig.json"), "{}").unwrap();

    // Switch to team B, whose server returns a row (team_id) but no artifacts.
    let body_b = serde_json::json!({
        "deployment_id": serde_json::Value::Null,
        "team_id": "team-b",
        "managed_config": serde_json::Value::Null,
        "requirements": serde_json::Value::Null,
    })
    .to_string();
    let (url_b, _auths_b) = spawn_mock(body_b);
    write_config(&home, &url_b);
    write_team_auth(&home, "team-b");

    let wrote = xai_grok_shell::managed_config::sync()
        .await
        .expect("team B sync must not fail the session");
    assert!(!wrote, "team B serves no artifacts, so nothing is written");

    // Team A's enforced policy is gone — team B does not inherit it.
    assert!(
        !home.join("requirements.toml").exists(),
        "team A's requirements must be evicted on the switch to team B"
    );
    assert!(
        !home.join("managed_config.toml").exists(),
        "team A's managed_config must be evicted on the switch to team B"
    );
    assert!(
        !home.join("managed_config.sig.json").exists(),
        "team A's stale sidecar must be evicted on the switch to team B"
    );

    // The marker is now team B's and must not claim B served A's artifacts.
    let marker = std::fs::read_to_string(home.join("managed_config_cache.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&marker).unwrap();
    assert_eq!(
        v["principal"].as_str(),
        Some("team-b"),
        "marker must rebind to team B: {marker}"
    );
    assert_eq!(
        v["had_requirements"].as_bool(),
        Some(false),
        "marker must not claim team B served requirements: {marker}"
    );
    assert_eq!(
        v["had_managed_config"].as_bool(),
        Some(false),
        "marker must not claim team B served managed_config: {marker}"
    );

    // Team B's cache reads fresh + identity-matched (no missing-artifact stale).
    assert!(!xai_grok_shell::config::is_managed_config_stale_for(
        &team_identity("team-b")
    ));
}

/// An artifact the server stops serving is removed on the next sync (disk converges
/// to the served set), and the marker stops claiming it — a withdrawn policy must not
/// keep enforcing from a stale file.
#[tokio::test]
#[serial]
async fn withdrawn_artifact_is_removed_on_next_sync() {
    let home = test_home().clone();
    reset(&home);

    // Sync 1: both artifacts served.
    let (url_full, _a) = spawn_mock(team_config_body());
    write_config(&home, &url_full);
    write_team_auth(&home, "team-007");
    xai_grok_shell::managed_config::sync()
        .await
        .expect("initial sync should succeed");
    assert!(home.join("requirements.toml").exists());

    // Sync 2: same team, requirements withdrawn.
    let body = serde_json::json!({
        "deployment_id": serde_json::Value::Null,
        "team_id": "team-007",
        "managed_config": TEAM_MANAGED,
        "requirements": serde_json::Value::Null,
    })
    .to_string();
    let (url_partial, _a2) = spawn_mock(body);
    write_config(&home, &url_partial);
    let wrote = xai_grok_shell::managed_config::sync()
        .await
        .expect("second sync should succeed");
    assert!(wrote, "removing the withdrawn artifact is a change");

    assert!(
        home.join("managed_config.toml").exists(),
        "the still-served artifact stays"
    );
    assert!(
        !home.join("requirements.toml").exists(),
        "the withdrawn artifact is removed"
    );
    let marker = std::fs::read_to_string(home.join("managed_config_cache.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&marker).unwrap();
    assert_eq!(
        v["had_requirements"].as_bool(),
        Some(false),
        "the marker stops claiming the withdrawn artifact: {marker}"
    );
    assert!(
        !xai_grok_shell::config::is_managed_config_stale_for(&team_identity("team-007")),
        "the converged cache is not stale"
    );
}

/// An empty dk response (`{}`) with a team signed in falls through to the team WITHOUT
/// applying: applying converges disk to the served (empty) set, which would delete the
/// team's files right before the team apply — observable when the team fetch then fails.
#[tokio::test]
#[serial]
async fn empty_dk_response_with_failing_team_leaves_team_policy_intact() {
    let home = test_home().clone();
    reset(&home);

    // Seed the team's policy files + marker.
    let (url, _a) = spawn_mock(team_config_body());
    write_config(&home, &url);
    write_team_auth(&home, "team-007");
    xai_grok_shell::managed_config::sync()
        .await
        .expect("team seed sync should succeed");
    assert!(home.join("requirements.toml").exists());

    // dk serves an empty row; the team fetch then fails (5xx for every retry).
    let (url2, _c, auths) = spawn_mock_seq(vec![(200, "{}".into()), (500, "boom".into())]);
    std::fs::write(
        home.join("config.toml"),
        format!("[endpoints]\nmanaged_config_url = \"{url2}\"\ndeployment_key = \"dep-key\"\n"),
    )
    .unwrap();

    let err = xai_grok_shell::managed_config::sync()
        .await
        .expect_err("the team fetch fails after the dk fallthrough");
    assert!(err.is_retryable(), "5xx is a transient failure: {err}");
    assert_eq!(
        auths.lock().unwrap().first().map(String::as_str),
        Some("Bearer dep-key"),
        "the dk was consulted first"
    );

    assert!(
        home.join("requirements.toml").exists() && home.join("managed_config.toml").exists(),
        "the empty dk body must not be applied (it would delete the team's files)"
    );
    let marker = std::fs::read_to_string(home.join("managed_config_cache.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&marker).unwrap();
    assert_eq!(
        v["principal"].as_str(),
        Some("team-007"),
        "the failed sync must not rewrite the marker: {marker}"
    );
}

/// A served-then-deleted artifact reads stale for the active identity; the session-start refresh refetches it.
#[tokio::test]
#[serial]
async fn served_then_deleted_refetches_best_effort() {
    let home = test_home().clone();
    reset(&home);

    let (url, _auths) = spawn_mock(team_config_body());
    write_config(&home, &url);
    write_team_auth(&home, "team-007");
    xai_grok_shell::managed_config::sync()
        .await
        .expect("initial sync should succeed");
    assert!(home.join("requirements.toml").exists());
    assert!(
        !xai_grok_shell::config::is_managed_config_stale_for(&team_identity("team-007")),
        "a fresh, identity-matched, complete cache is not stale"
    );

    // Tamper: delete the served file but keep the fresh marker.
    std::fs::remove_file(home.join("requirements.toml")).unwrap();
    assert!(
        xai_grok_shell::config::is_managed_config_stale_for(&team_identity("team-007")),
        "a served-but-now-missing artifact must read stale"
    );
    // Identity mismatch also reads stale (team switch).
    assert!(xai_grok_shell::config::is_managed_config_stale_for(
        &team_identity("team-other")
    ));

    // Best-effort refresh restores it; the session is never refused. Here the on-disk token is unexpired
    // (cache hit). The expired-refreshable path is covered by `expired_refreshable_team_token_heals_after_auth_refresh`.
    let auth_manager = std::sync::Arc::new(xai_grok_shell::auth::AuthManager::new(
        &home,
        xai_grok_shell::auth::GrokComConfig::default(),
    ));
    xai_grok_shell::managed_config::ensure_managed_policy_present(&auth_manager).await;
    assert!(
        home.join("requirements.toml").exists(),
        "the best-effort refresh restored the deleted artifact"
    );
    assert!(!xai_grok_shell::config::is_managed_config_stale_for(
        &team_identity("team-007")
    ));
}

/// An expired-but-refreshable team token with a served-then-deleted artifact must heal at session start.
/// The heal drives `auth()` first so the refreshed principal can refetch; else the expiry filters drop it unhealed.
#[tokio::test]
#[serial]
async fn expired_refreshable_team_token_heals_after_auth_refresh() {
    let home = test_home().clone();
    reset(&home);

    let (url, auths) = spawn_mock(team_config_body());
    // Point both the managed-config fetch and the proxy (post-refresh `/user`) at the mock — no production calls.
    let base = url.trim_end_matches("/deployment/config");
    std::fs::write(
        home.join("config.toml"),
        format!(
            "[endpoints]\nmanaged_config_url = \"{url}\"\ncli_chat_proxy_base_url = \"{base}\"\n"
        ),
    )
    .unwrap();

    // Establish a served, identity-matched cache (writes the sync marker).
    write_team_auth(&home, "team-007");
    xai_grok_shell::managed_config::sync()
        .await
        .expect("initial sync should succeed");
    assert!(home.join("requirements.toml").exists());

    // Cold-start brick: expired-but-refreshable on-disk token, served requirements.toml gone but still in the marker.
    write_expired_external_team_auth(&home, "team-007");
    std::fs::remove_file(home.join("requirements.toml")).unwrap();
    assert!(
        !xai_grok_shell::managed_config::has_principal(),
        "the expired token leaves no eligible managed principal — the expiry-filtered heal path can't see it (the brick)"
    );
    assert!(
        xai_grok_shell::config::is_managed_config_stale_for(&team_identity("team-007")),
        "a served-but-now-missing artifact reads stale"
    );

    // A real AuthManager whose refresher mints a fresh team token, persisted by `auth()`.
    let auth_manager = std::sync::Arc::new(xai_grok_shell::auth::AuthManager::new(
        &home,
        xai_grok_shell::auth::GrokComConfig::default(),
    ));
    auth_manager.configure_refresher(
        Some(r#"echo '{"access_token":"refreshed-team-token","expires_in":3600}'"#.to_string()),
        None,
    );

    xai_grok_shell::managed_config::ensure_managed_policy_present(&auth_manager).await;

    // The refresh re-enabled the heal: policy restored, refetched with the fresh token.
    assert!(
        home.join("requirements.toml").exists(),
        "the token refresh let the best-effort heal restore the deleted policy"
    );
    assert!(
        xai_grok_shell::managed_config::has_principal(),
        "the refreshed token is a live, eligible managed (team) principal"
    );
    assert!(
        auths
            .lock()
            .unwrap()
            .iter()
            .any(|a| a == "Bearer refreshed-team-token"),
        "the heal refetch authenticated with the refreshed token"
    );
}

/// The boundary: when no refresh can succeed (offline / dead token), the expired token still fails closed —
/// `auth()` errs, the heal doesn't run, and the deleted policy is NOT restored. Unbricks ONLY on a real refresh.
#[tokio::test]
#[serial]
async fn expired_team_token_without_successful_refresh_stays_failed_closed() {
    let home = test_home().clone();
    reset(&home);

    let (url, _auths) = spawn_mock(team_config_body());
    write_config(&home, &url);
    write_team_auth(&home, "team-007");
    xai_grok_shell::managed_config::sync()
        .await
        .expect("initial sync should succeed");
    std::fs::remove_file(home.join("requirements.toml")).unwrap();
    write_expired_external_team_auth(&home, "team-007");

    // A refresher that always fails -> `auth()` cannot produce a principal.
    let auth_manager = std::sync::Arc::new(xai_grok_shell::auth::AuthManager::new(
        &home,
        xai_grok_shell::auth::GrokComConfig::default(),
    ));
    auth_manager.configure_refresher(Some("false".to_string()), None);

    xai_grok_shell::managed_config::ensure_managed_policy_present(&auth_manager).await;

    assert!(
        !home.join("requirements.toml").exists(),
        "with no successful refresh the expired token cannot heal (fail-closed)"
    );
}

/// `managed_policy_gate` refuses a managed session when its served policy was deleted and the refetch can't
/// restore it (offline); intact or config-less is allowed. Exercises the real sync → marker → gate path.
#[tokio::test]
#[serial]
async fn managed_policy_gate_fails_closed_on_deleted_policy_offline() {
    let home = test_home().clone();
    reset(&home);

    // Admin opts in by serving `fail_closed = true` (server-driven; no local env).
    let body = serde_json::json!({
        "deployment_id": serde_json::Value::Null,
        "team_id": "team-007",
        "managed_config": TEAM_MANAGED,
        "requirements": format!("fail_closed = true\n{TEAM_REQUIREMENTS}"),
    })
    .to_string();
    let (url, _auths) = spawn_mock(body);
    write_config(&home, &url);
    write_team_auth(&home, "team-007");
    xai_grok_shell::managed_config::sync()
        .await
        .expect("initial sync should succeed");
    // Intact, identity-matched policy → the gate proceeds.
    assert!(
        xai_grok_shell::managed_config::managed_policy_gate().is_ok(),
        "an intact served policy must not be refused"
    );

    // Tamper: delete the served file but keep the marker → gate fails closed.
    std::fs::remove_file(home.join("requirements.toml")).unwrap();
    assert!(
        xai_grok_shell::managed_config::managed_policy_gate().is_err(),
        "a served-then-deleted policy must fail closed"
    );

    // Offline: the 5xx refetch can't restore the file → gate stays fail-closed (far-future token makes auth a cache hit).
    let (err_url, _c, _a) = spawn_mock_seq(vec![(500, "{}".to_string())]);
    write_config(&home, &err_url);
    let auth_manager = std::sync::Arc::new(xai_grok_shell::auth::AuthManager::new(
        &home,
        xai_grok_shell::auth::GrokComConfig::default(),
    ));
    xai_grok_shell::managed_config::ensure_managed_policy_present(&auth_manager).await;
    assert!(
        !home.join("requirements.toml").exists(),
        "a failed refetch cannot restore the deleted policy"
    );
    assert!(
        xai_grok_shell::managed_config::managed_policy_gate().is_err(),
        "still missing after a failed refetch → gate stays fail-closed"
    );

    // A config-less principal (the server served nothing) is never refused.
    reset(&home);
    write_config(&home, &url);
    write_team_auth(&home, "team-007");
    xai_grok_shell::config::mark_managed_config_synced(xai_grok_shell::config::SyncMarker {
        principal: Some("team-007"),
        had_managed_config: false,
        had_requirements: false,
        key_fingerprint: None,
        fail_closed: false,
    });
    assert!(
        xai_grok_shell::managed_config::managed_policy_gate().is_ok(),
        "a config-less principal must not be refused"
    );
}

/// `bootstrap` must run the fail-closed gate (it is `bootstrap`'s first step): a compromised managed policy
/// must fail the whole bootstrap closed, not just the standalone `managed_policy_gate`. Guards against a
/// refactor that drops the gate call from `bootstrap` — which the gate's own tests would not catch.
#[tokio::test]
#[serial]
async fn bootstrap_fails_closed_when_managed_policy_compromised() {
    let home = test_home().clone();
    reset(&home);

    // Provision a fail_closed team install (both artifacts served), then tamper by deleting the served policy.
    let body = serde_json::json!({
        "deployment_id": serde_json::Value::Null,
        "team_id": "team-007",
        "managed_config": TEAM_MANAGED,
        "requirements": format!("fail_closed = true\n{TEAM_REQUIREMENTS}"),
    })
    .to_string();
    let (url, _auths) = spawn_mock(body);
    write_config(&home, &url);
    write_team_auth(&home, "team-007");
    xai_grok_shell::managed_config::sync()
        .await
        .expect("initial sync should succeed");
    std::fs::remove_file(home.join("requirements.toml")).unwrap();

    // The gate is bootstrap's first step, so it refuses before any config/model work.
    let cfg = xai_grok_shell::agent::config::Config::default();
    let auth_manager = std::sync::Arc::new(xai_grok_shell::auth::AuthManager::new(
        &home,
        xai_grok_shell::auth::GrokComConfig::default(),
    ));
    // `bootstrap`'s Ok type isn't `Debug`, so match rather than `expect_err`.
    let err = match xai_grok_shell::agent::init::bootstrap(&cfg, &auth_manager, None) {
        Err(e) => e,
        Ok(_) => {
            panic!("a compromised fail_closed policy must fail bootstrap closed, but it succeeded")
        }
    };
    assert!(
        err.contains("Managed policy is required for this account"),
        "bootstrap must fail via the managed-policy gate (proves bootstrap calls it); got: {err}"
    );
}

/// Live wiring guard: an offline `GROK_DEPLOYMENT_KEY` switch on a fail_closed install must FAIL CLOSED, else a
/// regression returning `None` silently disables deploy-key-switch detection. Same-key ALLOW checks the lib's own `blake3(KEY-AAA)` exactly.
#[tokio::test]
#[serial]
async fn managed_policy_gate_fails_closed_on_deployment_key_switch_offline() {
    let home = test_home().clone();
    reset(&home);

    // Provision a fail_closed deploy install bound to key A; both artifacts written, so the only tamper signal is the key fingerprint.
    let body = serde_json::json!({
        "deployment_id": "deploy-A",
        "managed_config": TEAM_MANAGED,
        "requirements": format!("fail_closed = true\n{TEAM_REQUIREMENTS}"),
    })
    .to_string();
    let (url, _auths) = spawn_mock(body);
    write_config(&home, &url);

    // SAFETY: #[serial] test; the env is restored before any assertion below.
    unsafe { std::env::set_var("GROK_DEPLOYMENT_KEY", "KEY-AAA") };
    xai_grok_shell::managed_config::sync()
        .await
        .expect("deployment-key sync should record the fail_closed marker");

    // The marker records the lib-computed fingerprint (never the raw key), full blake3 hex.
    let marker = std::fs::read_to_string(home.join("managed_config_cache.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&marker).unwrap();
    let fp_a = v["key_fingerprint"]
        .as_str()
        .expect("deploy-key sync records a key fingerprint")
        .to_string();
    let fail_closed_recorded = v["fail_closed"].as_bool().unwrap_or(false);

    // GROK_MANAGED_CONFIG=0 disables any incidental background fetch (the gate is sync anyway).
    // SAFETY: #[serial] test; restored before any assertion below.
    unsafe { std::env::set_var("GROK_MANAGED_CONFIG", "0") };

    // Same key A → matching fingerprint → ALLOW (exact-equality vs recorded blake3).
    let gate_same_key = xai_grok_shell::managed_config::managed_policy_gate();

    // Switch to a different key B → REFUSE: exercises the full offline wiring.
    // SAFETY: #[serial] test; restored immediately below.
    unsafe { std::env::set_var("GROK_DEPLOYMENT_KEY", "KEY-BBB") };
    let gate_switched_key = xai_grok_shell::managed_config::managed_policy_gate();

    // SAFETY: #[serial] test; restore env BEFORE asserting so a failed assert can't leak it to later tests.
    unsafe {
        std::env::remove_var("GROK_DEPLOYMENT_KEY");
        std::env::remove_var("GROK_MANAGED_CONFIG");
    }

    // blake3-256 hex is exactly 64 hex chars — pins the recorded format.
    assert!(
        fp_a.len() == 64 && fp_a.chars().all(|c| c.is_ascii_hexdigit()),
        "recorded key_fingerprint must be a full blake3 hex, not just non-empty: {marker}"
    );
    assert!(
        fail_closed_recorded,
        "marker must record fail_closed = true: {marker}"
    );
    assert!(
        home.join("requirements.toml").exists() && home.join("managed_config.toml").exists(),
        "both served artifacts must be present so the fingerprint is the only tamper signal"
    );
    assert!(
        !marker.contains("KEY-AAA"),
        "the raw deployment key must never be written to disk: {marker}"
    );
    assert!(
        gate_same_key.is_ok(),
        "same deployment key + intact fail_closed policy must be ALLOWED (proves recorded fp == fresh blake3(KEY-AAA))"
    );
    assert!(
        gate_switched_key.is_err(),
        "a deployment-key switch (different fingerprint) on a fail_closed machine must FAIL CLOSED offline"
    );
}

/// A leftover fail_closed marker from a prior managed stint must NOT lock out a user who has since signed out
/// (no deployment key, no team auth): the gate requires a present principal, so `managed_principal_present()`
/// short-circuits. Guards the worst-case regression — locking a normal user out of their own CLI.
#[test]
#[serial]
fn former_managed_user_signed_out_is_not_locked_out() {
    let home = test_home().clone();
    reset(&home);

    // No deployment key in config, and `reset` removed auth.json → no principal.
    std::fs::write(home.join("config.toml"), "[endpoints]\n").unwrap();
    // A stale, opted-in marker that reads tampered: it recorded a served requirements.toml that's now absent.
    xai_grok_shell::config::mark_managed_config_synced(xai_grok_shell::config::SyncMarker {
        principal: Some("team-007"),
        had_managed_config: false,
        had_requirements: true,
        key_fingerprint: None,
        fail_closed: true,
    });

    assert!(
        xai_grok_shell::managed_config::managed_policy_gate().is_ok(),
        "a signed-out user with a leftover fail_closed marker must not be refused (no principal to enforce)"
    );
}

/// An unreadable auth.json makes `managed_principal_present()` fail safe to "present", but with NO fail_closed
/// marker the gate still allows — the fail-safe must never lock out a personal user who has no managed policy.
#[test]
#[serial]
fn unreadable_auth_without_marker_is_not_refused() {
    let home = test_home().clone();
    reset(&home);

    std::fs::write(home.join("config.toml"), "[endpoints]\n").unwrap();
    std::fs::write(home.join("auth.json"), "{corrupt json").unwrap();
    // No managed_config_cache.json marker at all.

    assert!(
        xai_grok_shell::managed_config::managed_policy_gate().is_ok(),
        "unreadable auth (fail-safe present) with no fail_closed marker must not be refused"
    );
}

/// Deploy-key online heal: a fail_closed deploy install whose served requirements.toml was deleted heals at
/// session start when the refetch succeeds, and the gate then allows — the key path's mirror of the team heal.
#[tokio::test]
#[serial]
async fn deployment_key_served_then_deleted_heals_online() {
    let home = test_home().clone();
    reset(&home);

    let body = serde_json::json!({
        "deployment_id": "deploy-A",
        "managed_config": TEAM_MANAGED,
        "requirements": format!("fail_closed = true\n{TEAM_REQUIREMENTS}"),
    })
    .to_string();
    let (url, _auths) = spawn_mock(body);
    std::fs::write(
        home.join("config.toml"),
        format!("[endpoints]\nmanaged_config_url = \"{url}\"\ndeployment_key = \"KEY-AAA\"\n"),
    )
    .unwrap();
    xai_grok_shell::managed_config::sync()
        .await
        .expect("initial deploy-key sync should succeed");
    assert!(home.join("requirements.toml").exists());

    // Tamper: delete the served artifact (offline this would fail closed).
    std::fs::remove_file(home.join("requirements.toml")).unwrap();

    // The mock still serves, so the best-effort session-start refresh restores it; the gate then allows.
    let auth_manager = std::sync::Arc::new(xai_grok_shell::auth::AuthManager::new(
        &home,
        xai_grok_shell::auth::GrokComConfig::default(),
    ));
    xai_grok_shell::managed_config::ensure_managed_policy_present(&auth_manager).await;
    assert!(
        home.join("requirements.toml").exists(),
        "the online refetch must restore the deleted deploy-key policy"
    );
    assert!(
        xai_grok_shell::managed_config::managed_policy_gate().is_ok(),
        "after a successful heal the deploy-key gate must allow"
    );
}

/// A confirmed offline team switch (fail_closed team-A install, then team B signs in with no
/// network): the gate PURGES team A's now-foreign artifacts and marker, then PERMITS team B —
/// a legitimate switch is neither refused nor left running under team A's lingering policy.
#[tokio::test]
#[serial]
async fn identity_change_permits_offline_team_switch_and_purges_prior_team() {
    let home = test_home().clone();
    reset(&home);

    let body = serde_json::json!({
        "deployment_id": serde_json::Value::Null,
        "team_id": "team-a",
        "managed_config": TEAM_MANAGED,
        "requirements": format!("fail_closed = true\n{TEAM_REQUIREMENTS}"),
    })
    .to_string();
    let (url, _auths) = spawn_mock(body);
    write_config(&home, &url);
    write_team_auth(&home, "team-a");
    xai_grok_shell::managed_config::sync()
        .await
        .expect("team-a sync should succeed");
    assert!(home.join("requirements.toml").exists());
    assert!(
        xai_grok_shell::managed_config::managed_policy_gate().is_ok(),
        "team A's intact fail_closed policy must start"
    );

    // Switch to team B while OFFLINE: the 5xx server means the session-start refresh cannot
    // purge via the apply path; the gate's own purge must handle the switch.
    let (err_url, _c, _a) = spawn_mock_seq(vec![(500, "{}".to_string())]);
    write_config(&home, &err_url);
    write_team_auth(&home, "team-b");
    let auth_manager = std::sync::Arc::new(xai_grok_shell::auth::AuthManager::new(
        &home,
        xai_grok_shell::auth::GrokComConfig::default(),
    ));
    xai_grok_shell::managed_config::ensure_managed_policy_present(&auth_manager).await;

    assert!(
        xai_grok_shell::managed_config::managed_policy_gate().is_ok(),
        "a legitimate offline team switch must not fail closed"
    );
    for f in xai_grok_shell::managed_config::MANAGED_ARTIFACT_FILES
        .into_iter()
        .chain([xai_grok_config::MANAGED_CONFIG_CACHE_FILE])
    {
        assert!(
            !home.join(f).exists(),
            "team A's {f} must be purged on the switch"
        );
    }
}

/// The gate purge takes the managed-config lock best-effort and SKIPS on contention (the holder
/// owns the transition). While held, an A→B switch retains team A's files — and the gate still
/// permits (a pure identity mismatch is not gate-grade tamper); once released, the next gate
/// call purges — proving the skip was contention-driven, not a silent no-op.
#[tokio::test]
#[serial]
async fn gate_purge_skips_while_lock_contended() {
    let home = test_home().clone();
    reset(&home);

    let body = serde_json::json!({
        "deployment_id": serde_json::Value::Null,
        "team_id": "team-a",
        "managed_config": TEAM_MANAGED,
        "requirements": format!("fail_closed = true\n{TEAM_REQUIREMENTS}"),
    })
    .to_string();
    let (url, _auths) = spawn_mock(body);
    write_config(&home, &url);
    write_team_auth(&home, "team-a");
    xai_grok_shell::managed_config::sync()
        .await
        .expect("team A sync should succeed");
    assert!(home.join("requirements.toml").exists());
    assert!(home.join("managed_config_cache.json").exists());

    write_team_auth(&home, "team-b");

    // Hold the managed-config flock (the same lock the gate purge tries), so the purge skips.
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.join("managed_config.lock"))
        .unwrap();
    lock.lock().unwrap();

    assert!(
        xai_grok_shell::managed_config::managed_policy_gate().is_ok(),
        "a contended purge skip leaves a pure identity mismatch, which must not refuse"
    );
    assert!(
        home.join("requirements.toml").exists(),
        "team A requirements must be RETAINED while the lock is contended (purge skipped)"
    );
    assert!(
        home.join("managed_config_cache.json").exists(),
        "team A marker must be RETAINED while the lock is contended"
    );

    // Release the lock: the next gate call acquires it and purges team A.
    lock.unlock().unwrap();
    assert!(
        xai_grok_shell::managed_config::managed_policy_gate().is_ok(),
        "after the lock releases, the gate purges team A and team B starts"
    );
    assert!(
        !home.join("requirements.toml").exists(),
        "the uncontended gate purges team A's requirements"
    );
    assert!(
        !home.join("managed_config_cache.json").exists(),
        "the uncontended gate purges team A's marker"
    );
}

/// A TRANSIENT lock holder must not turn an offline team switch into a skipped purge:
/// the purge retries the lock once after 100ms (`PURGE_LOCK_RETRY_DELAY`), so a holder
/// that releases within that window (~20ms here) is absorbed and the SAME gate call
/// purges team A on the second attempt.
#[tokio::test]
#[serial]
async fn gate_purge_retries_past_a_transient_lock_holder() {
    let home = test_home().clone();
    reset(&home);

    let body = serde_json::json!({
        "deployment_id": serde_json::Value::Null,
        "team_id": "team-a",
        "managed_config": TEAM_MANAGED,
        "requirements": format!("fail_closed = true\n{TEAM_REQUIREMENTS}"),
    })
    .to_string();
    let (url, _auths) = spawn_mock(body);
    write_config(&home, &url);
    write_team_auth(&home, "team-a");
    xai_grok_shell::managed_config::sync()
        .await
        .expect("team A sync should succeed");
    assert!(home.join("requirements.toml").exists());

    write_team_auth(&home, "team-b");

    // Acquire the flock BEFORE the gate call, then hand it to a helper that releases
    // it ~20ms in — inside the purge's retry window.
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.join("managed_config.lock"))
        .unwrap();
    lock.lock().unwrap();
    let holder = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        drop(lock); // releases the flock
    });

    assert!(
        xai_grok_shell::managed_config::managed_policy_gate().is_ok(),
        "a pure identity mismatch never refuses, purged or not"
    );
    holder.join().unwrap();

    assert!(
        !home.join("requirements.toml").exists(),
        "one gate call must absorb the transient holder via the retry and purge team A"
    );
    assert!(
        !home
            .join(xai_grok_config::MANAGED_CONFIG_CACHE_FILE)
            .exists(),
        "team A's marker goes with the retried purge"
    );
}

/// A blank `team_id` in `auth.json` (a parse blip / malformed write) is "unknown", not a
/// distinct identity: the gate must NOT fail closed and the purge must NOT shed team A's
/// policy. Guards the blank→None map in `active_team_id_any_expiry` and the detector's
/// blank guard end to end.
#[tokio::test]
#[serial]
async fn blank_team_id_neither_fails_closed_nor_purges() {
    let home = test_home().clone();
    reset(&home);

    let body = serde_json::json!({
        "deployment_id": serde_json::Value::Null,
        "team_id": "team-a",
        "managed_config": TEAM_MANAGED,
        "requirements": format!("fail_closed = true\n{TEAM_REQUIREMENTS}"),
    })
    .to_string();
    let (url, _auths) = spawn_mock(body);
    write_config(&home, &url);
    write_team_auth(&home, "team-a");
    xai_grok_shell::managed_config::sync()
        .await
        .expect("team A sync should succeed");
    assert!(home.join("requirements.toml").exists());

    // auth.json now carries a team principal with a BLANK team_id.
    write_team_auth(&home, "");

    assert!(
        xai_grok_shell::managed_config::managed_policy_gate().is_ok(),
        "a blank team_id must read as unknown, not a foreign substituted cache"
    );
    assert!(
        home.join("requirements.toml").exists(),
        "a parse blip must not purge team A's enforced policy"
    );
    assert!(
        home.join("managed_config_cache.json").exists(),
        "the team A marker must be retained on a blank team_id"
    );
    assert!(
        matches!(
            xai_grok_shell::managed_config::current_serving_identity(),
            ServingIdentity::None
        ),
        "a blank team_id must resolve to no identity, not Team(\"\") (spurious refetch input)"
    );
}

/// The session-start gate reads no env: `GROK_MANAGED_CONFIG_FAIL_CLOSED=0` must NOT disarm a fail_closed
/// refusal (unlike the requirements-layer version check, which the env can only tighten). No local bypass.
#[tokio::test]
#[serial]
async fn fail_closed_env_cannot_disarm_the_gate() {
    let home = test_home().clone();
    reset(&home);

    let body = serde_json::json!({
        "deployment_id": "deploy-A",
        "managed_config": TEAM_MANAGED,
        "requirements": format!("fail_closed = true\n{TEAM_REQUIREMENTS}"),
    })
    .to_string();
    let (url, _auths) = spawn_mock(body);
    std::fs::write(
        home.join("config.toml"),
        format!("[endpoints]\nmanaged_config_url = \"{url}\"\ndeployment_key = \"KEY-AAA\"\n"),
    )
    .unwrap();
    xai_grok_shell::managed_config::sync()
        .await
        .expect("deploy-key sync should succeed");

    // Tamper: delete the served requirements (offline this fails closed).
    std::fs::remove_file(home.join("requirements.toml")).unwrap();

    // SAFETY: #[serial] test; both vars restored before the assertion below.
    unsafe {
        std::env::set_var("GROK_MANAGED_CONFIG", "0"); // offline (the gate is sync anyway)
        std::env::set_var("GROK_MANAGED_CONFIG_FAIL_CLOSED", "0"); // attempt a local disarm
    }
    let gate = xai_grok_shell::managed_config::managed_policy_gate();
    unsafe {
        std::env::remove_var("GROK_MANAGED_CONFIG");
        std::env::remove_var("GROK_MANAGED_CONFIG_FAIL_CLOSED");
    }

    assert!(
        gate.is_err(),
        "GROK_MANAGED_CONFIG_FAIL_CLOSED=0 must not disarm the session-start gate (no local bypass)"
    );
}

/// Full logout removes the team scope from `auth.json`; the post-logout clear
/// (what `perform_logout` runs) removes the orphaned team-sourced files.
#[tokio::test]
#[serial]
async fn logout_clears_team_config() {
    let home = test_home().clone();
    reset(&home);

    let (url, _last_auth) = spawn_mock(team_config_body());
    write_config(&home, &url);
    write_team_auth(&home, "team-007");
    xai_grok_shell::managed_config::sync()
        .await
        .expect("sync should succeed");
    assert!(home.join("managed_config.toml").exists());
    assert!(
        home.join("managed_config_cache.json").exists(),
        "a successful sync writes the sync-marker cache"
    );

    // `AuthManager::clear` deletes auth.json when the last scope is removed.
    std::fs::remove_file(home.join("auth.json")).unwrap();
    xai_grok_shell::managed_config::clear_orphan();

    assert!(
        !home.join("managed_config.toml").exists(),
        "team-sourced managed_config should be cleared on logout"
    );
    assert!(
        !home.join("requirements.toml").exists(),
        "enforced requirements should be cleared on logout"
    );
    assert!(
        !home.join("managed_config_cache.json").exists(),
        "the sync-marker cache should be cleared on logout too"
    );
}

/// Seed on-disk fail_closed policy for clear_orphan keep tests.
/// `with_managed_files` writes managed_config + sig sidecars.
/// `with_marker` stamps a fail_closed sync marker for team-ms-fail-closed.
fn seed_fail_closed_orphan_artifacts(
    home: &std::path::Path,
    with_managed_files: bool,
    with_marker: bool,
) {
    if with_managed_files {
        std::fs::write(home.join("managed_config.toml"), TEAM_MANAGED).unwrap();
        std::fs::write(home.join("managed_config.sig.json"), r#"{"key_id":"v1"}"#).unwrap();
        std::fs::write(home.join("managed_identity.sig.json"), r#"{"key_id":"v1"}"#).unwrap();
    }
    std::fs::write(
        home.join("requirements.toml"),
        format!("fail_closed = true\n{TEAM_REQUIREMENTS}"),
    )
    .unwrap();
    if with_marker {
        xai_grok_shell::config::mark_managed_config_synced(xai_grok_shell::config::SyncMarker {
            principal: Some("team-ms-fail-closed"),
            had_managed_config: with_managed_files,
            had_requirements: true,
            key_fingerprint: None,
            fail_closed: true,
        });
    }
}

/// fail_closed escape fix: personal (User) auth with leftover MS fail_closed
/// artifacts must NOT be wiped by `clear_orphan` — that was the offline
/// switch-to-personal escape (managed files deleted, session ALLOW unrestricted).
#[test]
#[serial]
fn clear_orphan_keeps_fail_closed_when_switched_to_personal() {
    let home = test_home().clone();
    reset(&home);
    seed_fail_closed_orphan_artifacts(&home, true, true);

    // Personal User principal (no team_id) — the escape repro.
    let scope = xai_grok_shell::auth::GrokComConfig::default().auth_scope();
    let auth = serde_json::json!({
        scope: {
            "key": "personal-token",
            "auth_mode": "oidc",
            "create_time": "2026-01-01T00:00:00Z",
            "expires_at": FAR_FUTURE,
            "user_id": "user-1",
        }
    });
    std::fs::write(home.join("auth.json"), auth.to_string()).unwrap();

    xai_grok_shell::managed_config::clear_orphan();

    assert!(
        home.join("requirements.toml").exists(),
        "fail_closed requirements must survive personal identity switch"
    );
    assert!(
        home.join("managed_config.toml").exists(),
        "fail_closed managed_config must survive personal identity switch"
    );
    assert!(
        home.join("managed_config.sig.json").exists(),
        "sig sidecar must survive personal identity switch under fail_closed"
    );
    assert!(
        home.join("managed_config_cache.json").exists(),
        "fail_closed marker must survive personal identity switch"
    );
}

/// Signed-out logout with fail_closed still keeps policy (same as personal switch).
#[test]
#[serial]
fn clear_orphan_keeps_fail_closed_when_signed_out() {
    let home = test_home().clone();
    reset(&home);
    seed_fail_closed_orphan_artifacts(&home, false, true);
    // No auth.json = signed out.
    xai_grok_shell::managed_config::clear_orphan();

    assert!(
        home.join("requirements.toml").exists(),
        "signed-out must not wipe fail_closed requirements"
    );
    assert!(
        home.join("managed_config_cache.json").exists(),
        "signed-out must not wipe fail_closed marker"
    );
}

/// Marker stripped but requirements still say fail_closed = true: still keep.
#[test]
#[serial]
fn clear_orphan_keeps_fail_closed_requirements_without_marker() {
    let home = test_home().clone();
    reset(&home);
    seed_fail_closed_orphan_artifacts(&home, false, false);
    // No marker, no team auth.
    xai_grok_shell::managed_config::clear_orphan();

    assert!(
        home.join("requirements.toml").exists(),
        "on-disk fail_closed requirements must be kept even without a marker"
    );
}

/// Unreadable requirements (PermissionDenied) with no fail_closed marker must
/// still keep artifacts — cannot confirm disarmed, so clear_orphan must not wipe.
#[test]
#[serial]
#[cfg(unix)]
fn clear_orphan_keeps_unreadable_requirements_without_marker() {
    use std::os::unix::fs::PermissionsExt;

    let home = test_home().clone();
    reset(&home);
    seed_fail_closed_orphan_artifacts(&home, true, false);
    // No fail_closed marker; requirements exist with fail_closed = true but will
    // be made unreadable so the flag cannot be parsed.
    let req = home.join("requirements.toml");
    std::fs::set_permissions(&req, std::fs::Permissions::from_mode(0o000)).unwrap();
    struct RestorePerms<'a>(&'a std::path::Path);
    impl Drop for RestorePerms<'_> {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o600));
        }
    }
    let _restore = RestorePerms(&req);

    assert!(
        xai_grok_config::fail_closed_policy_armed_at(&home),
        "unreadable requirements must arm fail_closed"
    );
    xai_grok_shell::managed_config::clear_orphan();

    // Restore so exists() / cleanup can inspect the tree.
    drop(_restore);
    assert!(
        home.join("requirements.toml").exists(),
        "unreadable requirements must not be wiped by clear_orphan"
    );
    assert!(
        home.join("managed_config.toml").exists(),
        "managed_config must survive when requirements are unreadable"
    );
}

/// An expired token for a still-signed-in team is not a logout: cold-start
/// tokens are routinely expired before refresh, so the clear is expiry-agnostic.
#[test]
#[serial]
fn cold_start_expired_token_keeps_config() {
    let home = test_home().clone();
    reset(&home);

    std::fs::write(home.join("managed_config.toml"), TEAM_MANAGED).unwrap();
    std::fs::write(home.join("requirements.toml"), TEAM_REQUIREMENTS).unwrap();
    write_team_auth_expiry(&home, "team-007", PAST);
    xai_grok_shell::managed_config::clear_orphan();

    assert!(
        home.join("managed_config.toml").exists(),
        "expired-but-present team token must not wipe enforced managed_config"
    );
    assert!(
        home.join("requirements.toml").exists(),
        "expired-but-present team token must not wipe enforced requirements"
    );
}

/// Fail-closed: an UNREADABLE (corrupt) auth.json is not a logout — the clear
/// must keep the team's enforced files until the read recovers.
#[test]
#[serial]
fn unreadable_auth_keeps_config() {
    let home = test_home().clone();
    reset(&home);

    std::fs::write(home.join("requirements.toml"), TEAM_REQUIREMENTS).unwrap();
    std::fs::write(home.join("auth.json"), "{corrupt json").unwrap();
    xai_grok_shell::managed_config::clear_orphan();

    assert!(
        home.join("requirements.toml").exists(),
        "an unreadable auth.json must not wipe enforced policy"
    );
}

#[tokio::test]
#[serial]
async fn deployment_key_wins_over_team_when_both_present() {
    let home = test_home().clone();
    reset(&home);

    let (url, auths) = spawn_mock(team_config_body());
    std::fs::write(
        home.join("config.toml"),
        format!("[endpoints]\nmanaged_config_url = \"{url}\"\ndeployment_key = \"dep-key-123\"\n"),
    )
    .unwrap();
    write_team_auth(&home, "team-007");

    let wrote = xai_grok_shell::managed_config::sync()
        .await
        .expect("sync should succeed");
    assert!(wrote);
    assert_eq!(
        auths.lock().unwrap().last().map(String::as_str),
        Some("Bearer dep-key-123"),
        "deployment key must win over the team token"
    );
}

/// A successful deploy-key sync records the served `deployment_id` as `principal` and a non-empty
/// `key_fingerprint` (one-way hash), never the raw key — so a switched key stops serving the prior config.
#[tokio::test]
#[serial]
async fn deployment_key_sync_records_principal_and_key_fingerprint() {
    let home = test_home().clone();
    reset(&home);

    let body = serde_json::json!({
        "deployment_id": "dep-42",
        "managed_config": "[cli]\ntheme = \"dark\"\n",
        "requirements": "[features]\nweb_fetch = false\n",
    })
    .to_string();
    let (url, _auths) = spawn_mock(body);
    std::fs::write(
        home.join("config.toml"),
        format!(
            "[endpoints]\nmanaged_config_url = \"{url}\"\ndeployment_key = \"dep-key-secret\"\n"
        ),
    )
    .unwrap();

    let wrote = xai_grok_shell::managed_config::sync()
        .await
        .expect("deployment-key sync should succeed");
    assert!(wrote);

    let marker = std::fs::read_to_string(home.join("managed_config_cache.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&marker).unwrap();
    assert_eq!(
        v["principal"].as_str(),
        Some("dep-42"),
        "deploy-key marker records the served deployment_id as principal: {marker}"
    );
    let fp = v["key_fingerprint"]
        .as_str()
        .expect("deploy-key marker records a key fingerprint");
    assert!(!fp.is_empty(), "the key fingerprint must be non-empty");
    assert!(
        !marker.contains("dep-key-secret"),
        "the raw deployment key must never be written to disk: {marker}"
    );
}

/// A configured deployment key keeps its files even with no team signed in —
/// the orphan clear must never delete a deployment-key install's config.
#[test]
#[serial]
fn deployment_key_config_survives_clear() {
    let home = test_home().clone();
    reset(&home);

    // A deployment-key install: files present + the key persisted in config.toml.
    std::fs::write(
        home.join("config.toml"),
        "[endpoints]\ndeployment_key = \"dep-key-123\"\n",
    )
    .unwrap();
    std::fs::write(
        home.join("managed_config.toml"),
        "[cli]\ninstaller = \"internal\"\n",
    )
    .unwrap();
    let _ = std::fs::remove_file(home.join("auth.json"));

    xai_grok_shell::managed_config::clear_orphan();

    assert!(
        home.join("managed_config.toml").exists(),
        "deployment-key managed_config must survive the orphan clear"
    );
}

/// A personal (non-team) OAuth login is not eligible: no bearer sent, nothing
/// written. Guards the `is_team_principal()` eligibility check.
#[tokio::test]
#[serial]
async fn personal_login_is_noop() {
    let home = test_home().clone();
    reset(&home);

    let (url, count, _) = spawn_mock_seq(vec![(200, team_config_body())]);
    write_config(&home, &url);
    // A signed-in USER principal (no team_id, principal_type absent).
    let scope = xai_grok_shell::auth::GrokComConfig::default().auth_scope();
    let auth = serde_json::json!({
        scope: {
            "key": "personal-token",
            "auth_mode": "oidc",
            "create_time": "2026-01-01T00:00:00Z",
            "expires_at": FAR_FUTURE,
            "user_id": "user-1",
        }
    });
    std::fs::write(home.join("auth.json"), auth.to_string()).unwrap();

    let wrote = xai_grok_shell::managed_config::sync()
        .await
        .expect("personal login → no-op, not an error");
    assert!(!wrote, "a personal login must not fetch team config");
    assert_eq!(*count.lock().unwrap(), 0, "no bearer sent to the endpoint");
    assert!(!home.join("managed_config.toml").exists());
}

/// Security guard: a lock-skipped apply (dk row HAS config) must not be read as
/// an empty row and fall through to the team token on a deployment-key machine.
#[tokio::test]
#[serial]
async fn lock_contention_does_not_fall_through_to_team() {
    let home = test_home().clone();
    reset(&home);

    // dk row returns real config; a team principal is also present.
    let (url, count, auths) = spawn_mock_seq(vec![(200, team_config_body())]);
    std::fs::write(
        home.join("config.toml"),
        format!("[endpoints]\nmanaged_config_url = \"{url}\"\ndeployment_key = \"dep-key\"\n"),
    )
    .unwrap();
    write_team_auth(&home, "team-007");

    // Hold the managed-config lock (same flock the client uses) so apply_fetched
    // skips and returns Ok(false).
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.join("managed_config.lock"))
        .unwrap();
    lock.lock().unwrap();

    let wrote = xai_grok_shell::managed_config::sync()
        .await
        .expect("contended sync is a no-op, not an error");
    lock.unlock().unwrap();

    assert!(!wrote, "nothing applied while the lock is held");
    // The dk fetch happened; the team token must NOT have been tried as a
    // fallthrough (that would fetch the team's config onto a dk machine).
    assert_eq!(
        auths.lock().unwrap().as_slice(),
        ["Bearer dep-key"],
        "contention must not trigger the dk->team fallthrough"
    );
    assert_eq!(*count.lock().unwrap(), 1);
}

/// `grok setup` with config served but the lock held by another writer reports the
/// skip: not Installed (THIS run persisted nothing) and not NothingConfigured (the
/// server does have config).
#[tokio::test]
#[serial]
async fn setup_lock_skip_is_not_reported_as_no_config() {
    let home = test_home().clone();
    reset(&home);

    let (url, _auths) = spawn_mock(team_config_body());
    write_config(&home, &url);
    write_team_auth(&home, "team-007");

    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.join("managed_config.lock"))
        .unwrap();
    lock.lock().unwrap();

    let outcome = xai_grok_shell::managed_config::run_setup().await;
    lock.unlock().unwrap();

    assert!(
        matches!(
            outcome,
            xai_grok_shell::managed_config::SetupOutcome::Skipped
        ),
        "a lock skip persisted nothing: it must report Skipped, not Installed or \
         NothingConfigured, got {outcome:?}"
    );
}

/// A transient (5xx) failure is retried; once the server recovers, the config
/// is written. (Backoff is overridden to 10ms in `test_home`.)
#[tokio::test]
#[serial]
async fn sync_retries_after_transient_error() {
    let home = test_home().clone();
    reset(&home);

    let (url, count, _auths) =
        spawn_mock_seq(vec![(500, "boom".into()), (200, team_config_body())]);
    write_config(&home, &url);
    write_team_auth(&home, "team-007");

    let wrote = xai_grok_shell::managed_config::sync()
        .await
        .expect("should retry the 500 then succeed");
    assert!(wrote);
    assert!(home.join("managed_config.toml").exists());
    assert!(
        *count.lock().unwrap() >= 2,
        "expected a retry after the transient 500"
    );
}

/// A body-phase interruption — the server writes valid headers then drops before the
/// body completes ("connection closed before message completed", a `reqwest` body error,
/// NOT a decode error) — must be classified transient and recovered on a fresh
/// connection. Pre-fix this mapped to non-retryable `InvalidResponse` and would NOT
/// retry; here `sync()` succeeds. (Backoff is 10ms via `test_home`.)
#[tokio::test]
#[serial]
async fn sync_retries_after_body_phase_drop() {
    let home = test_home().clone();
    reset(&home);

    let (url, count) = spawn_mock_truncating_body_first(1, team_config_body());
    write_config(&home, &url);
    write_team_auth(&home, "team-007");

    let wrote = xai_grok_shell::managed_config::sync()
        .await
        .expect("a mid-body drop must be retried on a fresh connection, then succeed");
    assert!(wrote);
    assert!(home.join("managed_config.toml").exists());
    assert!(
        *count.lock().unwrap() >= 2,
        "expected a retry on a new connection after the first body read was interrupted"
    );
}

/// Classification: an in-flight connection interruption must NOT be misreported as
/// an unreachable server — the error must not blame the user's network (the bug),
/// and must surface the transient-interruption wording instead.
#[tokio::test]
#[serial]
async fn connection_drop_is_not_reported_as_unreachable() {
    let home = test_home().clone();
    reset(&home);

    // Every connection is dropped mid-flight, so all retries fail identically.
    let (url, _count) = spawn_mock_closing_first(usize::MAX, team_config_body());
    write_config(&home, &url);
    write_team_auth(&home, "team-007");

    let err = xai_grok_shell::managed_config::sync()
        .await
        .expect_err("all connections dropped → the fetch fails");
    let msg = err.to_string().to_lowercase();
    assert!(
        !msg.contains("check your network"),
        "an in-flight interruption must not be misreported as unreachable: {msg}"
    );
    assert!(
        msg.contains("interrupted") || msg.contains("timed out"),
        "the message must describe a transient connection interruption: {msg}"
    );
}

/// The payload side of the body split: a 200 with a non-JSON body is a malformed payload, not a
/// transport interruption — it must fail TERMINALLY (`InvalidResponse`), not retry. Guards the
/// `from_slice` arm of the split; the transport arm is covered by `sync_retries_after_body_phase_drop`.
#[tokio::test]
#[serial]
async fn sync_fails_terminally_on_malformed_payload() {
    let home = test_home().clone();
    reset(&home);

    // A 200 whose body is not JSON (e.g. an HTML error page slipped through a proxy).
    let (url, count, _auths) = spawn_mock_seq(vec![(200, "<html>not json</html>".into())]);
    write_config(&home, &url);
    write_team_auth(&home, "team-007");

    let err = xai_grok_shell::managed_config::sync()
        .await
        .expect_err("a malformed payload must fail, not write config");
    assert_eq!(
        *count.lock().unwrap(),
        1,
        "a malformed payload is terminal and must not be retried"
    );
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("unexpected response"),
        "a malformed payload must surface as an unexpected-response error, not a transient one: {msg}"
    );
    assert!(!home.join("managed_config.toml").exists());
}

// note: this asserts the POOLING half of the `shared_client()` tuning. Evicting/recovering a
// half-dead h2 connection (and the `send_with_retry_escaping_pool` fresh-final-attempt escape) isn't
// deterministically simulable with the std `TcpListener` harness, so it's covered by the e2e pass.

/// The tuned pooled `shared_client()` reuses one TCP connection across back-to-back requests (pool
/// eviction is time-based, so two quick requests share a connection). Regression guard against a
/// future mis-tuning that disables pooling for the general-purpose client.
#[tokio::test]
#[serial]
async fn shared_client_reuses_pooled_connection() {
    // Scrub the suite's env (notably HTTP(S)_PROXY) before `shared_client()` is built, so a proxy
    // can't intercept the 127.0.0.1 mock and break the accept count regardless of test ordering.
    let _ = test_home();
    let (base_url, accepts, _heads) = spawn_counting_server().await;
    let client = xai_grok_shell::http::shared_client();

    client
        .get(base_url.as_str())
        .send()
        .await
        .expect("first request succeeds")
        .bytes()
        .await
        .expect("first body reads to completion");
    // Brief pause so the idle connection is checked back into the pool before the second request.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    client
        .get(base_url.as_str())
        .send()
        .await
        .expect("second request succeeds")
        .bytes()
        .await
        .expect("second body reads to completion");

    assert_eq!(
        accepts.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the pooled client must reuse one TCP connection across back-to-back requests"
    );
}

/// An auth rejection is terminal: fail fast with the team-tailored message and
/// no retries (a bad credential won't fix itself by retrying).
#[tokio::test]
#[serial]
async fn sync_fails_fast_on_auth_error_without_retry() {
    let home = test_home().clone();
    reset(&home);

    let (url, count, _auths) = spawn_mock_seq(vec![(401, "{\"error\":\"nope\"}".into())]);
    write_config(&home, &url);
    write_team_auth(&home, "team-007");

    let err = xai_grok_shell::managed_config::sync()
        .await
        .expect_err("401 should be an error");
    assert_eq!(*count.lock().unwrap(), 1, "auth error must not be retried");
    let msg = err.to_string().to_lowercase();
    assert!(msg.contains("team sign-in"), "team-tailored message: {msg}");
}

/// A rejected deployment key (stale env/config leftover) must not starve a
/// valid team sign-in: the sync falls back to the team session token.
#[tokio::test]
#[serial]
async fn rejected_deployment_key_falls_back_to_team() {
    let home = test_home().clone();
    reset(&home);

    let (url, count, auths) = spawn_mock_seq(vec![
        (401, "{\"error\":\"bad key\"}".into()), // deployment key attempt
        (200, team_config_body()),               // team fallback
    ]);
    std::fs::write(
        home.join("config.toml"),
        format!("[endpoints]\nmanaged_config_url = \"{url}\"\ndeployment_key = \"stale-key\"\n"),
    )
    .unwrap();
    write_team_auth(&home, "team-007");

    let wrote = xai_grok_shell::managed_config::sync()
        .await
        .expect("team fallback should succeed after the key is rejected");

    assert!(wrote);
    assert_eq!(*count.lock().unwrap(), 2, "key attempt + team attempt");
    assert_eq!(
        auths.lock().unwrap().as_slice(),
        ["Bearer stale-key", "Bearer team-session-token"],
        "first the rejected key, then the team token"
    );
}

/// A deployment key whose response is EMPTY (no row provisioned) must not
/// starve the signed-in team: the sync falls through to the team token.
#[tokio::test]
#[serial]
async fn empty_deployment_response_falls_through_to_team() {
    let home = test_home().clone();
    reset(&home);

    let (url, count, auths) = spawn_mock_seq(vec![
        (200, "{}".into()),        // deployment key: no row
        (200, team_config_body()), // team fallback
    ]);
    std::fs::write(
        home.join("config.toml"),
        format!("[endpoints]\nmanaged_config_url = \"{url}\"\ndeployment_key = \"dep-key\"\n"),
    )
    .unwrap();
    write_team_auth(&home, "team-007");

    let wrote = xai_grok_shell::managed_config::sync()
        .await
        .expect("team fallthrough should succeed");

    assert!(wrote);
    assert_eq!(*count.lock().unwrap(), 2);
    assert_eq!(
        auths.lock().unwrap().as_slice(),
        ["Bearer dep-key", "Bearer team-session-token"]
    );
}

/// A deployment row with empty content (echoed `deployment_id`) still owns the
/// machine: fallthrough gates on existence, not content, so the team token isn't
/// tried.
#[tokio::test]
#[serial]
async fn empty_content_deployment_row_does_not_fall_through_to_team() {
    let home = test_home().clone();
    reset(&home);

    // 200 with a row (deployment_id present) but empty content; team config is
    // queued second so a wrong fallthrough would be observable.
    let degraded = serde_json::json!({
        "deployment_id": "dep-1",
        "managed_config": "",
        "requirements": serde_json::Value::Null,
    })
    .to_string();
    let (url, count, auths) = spawn_mock_seq(vec![(200, degraded), (200, team_config_body())]);
    std::fs::write(
        home.join("config.toml"),
        format!("[endpoints]\nmanaged_config_url = \"{url}\"\ndeployment_key = \"dep-key\"\n"),
    )
    .unwrap();
    write_team_auth(&home, "team-007");

    let wrote = xai_grok_shell::managed_config::sync()
        .await
        .expect("degraded dk row is a no-op, not an error");

    assert!(!wrote, "empty content writes nothing");
    assert_eq!(
        *count.lock().unwrap(),
        1,
        "a real dk row must not trigger the team fetch"
    );
    assert_eq!(
        auths.lock().unwrap().as_slice(),
        ["Bearer dep-key"],
        "a provisioned-but-empty dk row must not fall through to the team token"
    );
    assert!(!home.join("managed_config.toml").exists());
}

/// `GROK_MANAGED_CONFIG=0` is an explicit opt-out: the post-login sync must
/// make zero requests.
#[tokio::test]
#[serial]
async fn managed_config_opt_out_makes_no_requests() {
    let home = test_home().clone();
    reset(&home);

    let (url, count, _) = spawn_mock_seq(vec![(200, team_config_body())]);
    write_config(&home, &url);
    write_team_auth(&home, "team-007");

    // SAFETY: #[serial] test; restored before returning.
    unsafe { std::env::set_var("GROK_MANAGED_CONFIG", "0") };
    let outcome = xai_grok_shell::managed_config::post_login_sync(None).await;
    unsafe { std::env::remove_var("GROK_MANAGED_CONFIG") };

    assert_eq!(
        outcome,
        xai_grok_shell::managed_config::ManagedConfigSync::Skipped
    );

    assert_eq!(*count.lock().unwrap(), 0, "opt-out must suppress the fetch");
    assert!(!home.join("managed_config.toml").exists());
}

/// Post-login pins the just-authenticated principal over a different on-disk
/// team — a login to team B can't sync team A's policy.
#[tokio::test]
#[serial]
async fn post_login_pins_authenticated_team_over_disk() {
    let home = test_home().clone();
    reset(&home);

    let (url, auths) = spawn_mock(team_config_body());
    write_config(&home, &url);
    // The on-disk "current" team uses the default "team-session-token".
    write_team_auth(&home, "team-disk");

    // But we just authenticated as a different team with a distinct token.
    let pinned: xai_grok_shell::auth::GrokAuth = serde_json::from_value(serde_json::json!({
        "key": "pinned-token",
        "auth_mode": "oidc",
        "create_time": "2026-01-01T00:00:00Z",
        "expires_at": FAR_FUTURE,
        "user_id": "user-1",
        "principal_type": "Team",
        "team_id": "team-pinned",
    }))
    .unwrap();

    let outcome = xai_grok_shell::managed_config::post_login_sync(Some(pinned)).await;
    assert_eq!(
        outcome,
        xai_grok_shell::managed_config::ManagedConfigSync::Updated { is_team: true }
    );
    assert_eq!(
        auths.lock().unwrap().last().map(String::as_str),
        Some("Bearer pinned-token"),
        "must authenticate as the pinned principal, not the on-disk current team"
    );
}

/// The login-path sync stops after its small retry budget (2), not the full
/// background budget (5), and never surfaces an error.
#[tokio::test]
#[serial]
async fn post_login_sync_is_latency_bounded() {
    let home = test_home().clone();
    reset(&home);

    let (url, count, _auths) = spawn_mock_seq(vec![(500, "boom".into())]);
    write_config(&home, &url);
    write_team_auth(&home, "team-007");

    let outcome = xai_grok_shell::managed_config::post_login_sync(None).await;

    assert_eq!(
        outcome,
        xai_grok_shell::managed_config::ManagedConfigSync::Failed
    );
    assert_eq!(
        *count.lock().unwrap(),
        2,
        "login sync must stop after its bounded retry budget"
    );
    assert!(!home.join("managed_config.toml").exists());
}

/// A deploy key is local config any process can write, not a signed-in identity — so on a dk
/// machine the gate purge must never fire, even when `auth.json` shows a confirmed team switch
/// underneath: purging would let any local process shed the key's policy offline.
#[tokio::test]
#[serial]
async fn deploy_key_machine_never_gate_purges_on_team_switch() {
    let home = test_home().clone();
    reset(&home);

    // Sync as team A, then go offline and switch auth.json to team B — the purge-eligible shape.
    let body = serde_json::json!({
        "deployment_id": serde_json::Value::Null,
        "team_id": "team-a",
        "managed_config": TEAM_MANAGED,
        "requirements": TEAM_REQUIREMENTS,
    })
    .to_string();
    let (url, _auths) = spawn_mock(body);
    write_config(&home, &url);
    write_team_auth(&home, "team-a");
    xai_grok_shell::managed_config::sync()
        .await
        .expect("team-a sync should succeed");
    assert!(home.join("managed_config.toml").exists());

    // Same switch as the purging sibling test, but with a deployment key configured: the
    // serving identity resolves to the key, so the Team-only purge path must not run.
    let (err_url, _c, _a) = spawn_mock_seq(vec![(500, "{}".to_string())]);
    std::fs::write(
        home.join("config.toml"),
        format!(
            "[endpoints]\nmanaged_config_url = \"{err_url}\"\ndeployment_key = \"dk-under-test\"\n"
        ),
    )
    .unwrap();
    write_team_auth(&home, "team-b");
    let auth_manager = std::sync::Arc::new(xai_grok_shell::auth::AuthManager::new(
        &home,
        xai_grok_shell::auth::GrokComConfig::default(),
    ));
    xai_grok_shell::managed_config::ensure_managed_policy_present(&auth_manager).await;

    // The gate is the purge's only caller — without this call the guard is unexercised.
    assert!(
        xai_grok_shell::managed_config::managed_policy_gate().is_ok(),
        "dk gate must permit"
    );
    assert!(
        home.join("managed_config.toml").exists(),
        "a dk machine must keep its policy across an auth.json team flip"
    );
    assert!(
        home.join("managed_config_cache.json").exists(),
        "the sync marker must survive too — the key, not the team, owns this machine's policy"
    );
}

/// For every on-disk state a crashed purge can leave (each proper prefix of the removal
/// order), the marker is still present, the detector still fires for the new team, and a
/// later purge converges. The order itself is pinned by `marker_is_not_a_managed_artifact`
/// plus the fault-injection unit test.
#[tokio::test]
#[serial]
async fn purge_crash_prefixes_stay_armed_and_converge() {
    let home = test_home().clone();
    let artifacts = xai_grok_shell::managed_config::MANAGED_ARTIFACT_FILES;
    // 0..=len: every proper prefix of the 4-step removal order, up to and including
    // "all artifacts removed, marker still present" (a crash right before the marker step).
    for prefix_len in 0..=artifacts.len() {
        reset(&home);
        let body = serde_json::json!({
            "deployment_id": serde_json::Value::Null,
            "team_id": "team-a",
            "managed_config": TEAM_MANAGED,
            "requirements": format!("fail_closed = true\n{TEAM_REQUIREMENTS}"),
        })
        .to_string();
        let (url, _auths) = spawn_mock(body);
        write_config(&home, &url);
        write_team_auth(&home, "team-a");
        xai_grok_shell::managed_config::sync()
            .await
            .expect("team-a sync should succeed");

        // Simulate a purge crashed after removing only this prefix.
        for name in &artifacts[..prefix_len] {
            let _ = std::fs::remove_file(home.join(name));
        }
        assert!(
            home.join(xai_grok_config::MANAGED_CONFIG_CACHE_FILE)
                .exists(),
            "marker must outlive every artifact prefix (prefix_len={prefix_len})"
        );

        // Team B arrives offline: the detector must still confirm and the purge converge.
        write_team_auth(&home, "team-b");
        assert_eq!(
            xai_grok_config::confirmed_team_switch("team-b").as_deref(),
            Some("team-a"),
            "detector must stay armed after a crash prefix (prefix_len={prefix_len})"
        );
        assert!(
            xai_grok_shell::managed_config::managed_policy_gate().is_ok(),
            "offline switch over a crash prefix must not refuse (prefix_len={prefix_len})"
        );
        for name in artifacts {
            assert!(
                !home.join(name).exists(),
                "{name} must be purged (prefix_len={prefix_len})"
            );
        }
        assert!(
            !home
                .join(xai_grok_config::MANAGED_CONFIG_CACHE_FILE)
                .exists(),
            "the converged purge drops the marker last (prefix_len={prefix_len})"
        );
    }
}

/// Marker written under the apply lock by the holder only: lock-contended apply records nothing.
#[tokio::test]
#[serial]
async fn contended_sync_writes_no_marker() {
    let home = test_home().clone();
    reset(&home);

    let body = serde_json::json!({
        "deployment_id": serde_json::Value::Null,
        "team_id": "team-a",
        "managed_config": TEAM_MANAGED,
        "requirements": TEAM_REQUIREMENTS,
    })
    .to_string();
    let (url, auths) = spawn_mock(body);
    write_config(&home, &url);
    write_team_auth(&home, "team-a");

    // Hold the managed-config flock across the sync: apply skips, so nothing is persisted.
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.join("managed_config.lock"))
        .unwrap();
    lock.lock().unwrap();
    let synced = xai_grok_shell::managed_config::sync()
        .await
        .expect("sync should succeed (skip, not error)");
    assert!(!synced, "a lock-contended apply must not report a write");
    lock.unlock().unwrap();

    // Positive control: the FETCH happened (only the apply was skipped), so the
    // no-marker assertions below can't pass vacuously on a sync that never ran.
    assert!(
        !auths.lock().unwrap().is_empty(),
        "the fetch must have reached the server"
    );
    assert!(
        !home
            .join(xai_grok_config::MANAGED_CONFIG_CACHE_FILE)
            .exists(),
        "a contended sync must not write a marker for files it never persisted"
    );
    assert!(!home.join("requirements.toml").exists());
}

/// Credential vanished mid-fetch → apply Skipped, no marker (sibling of contention skip).
#[tokio::test]
#[serial]
async fn credential_gone_mid_fetch_writes_no_marker() {
    let home = test_home().clone();
    reset(&home);

    let body = serde_json::json!({
        "deployment_id": "dep-1",
        "managed_config": TEAM_MANAGED,
        "requirements": TEAM_REQUIREMENTS,
    })
    .to_string();
    // Delay the response so we can clear the deployment key after the fetch starts
    // but before apply runs.
    let (url, count, auths) = spawn_mock_delayed(body, std::time::Duration::from_millis(200));
    std::fs::write(
        home.join("config.toml"),
        format!(
            "[endpoints]\nmanaged_config_url = \"{url}\"\ndeployment_key = \"KEY-GOING-AWAY\"\n"
        ),
    )
    .unwrap();

    let home_for_clear = home.clone();
    let clearer = std::thread::spawn(move || {
        // Wait until the mock has accepted a request, then drop the key.
        for _ in 0..50 {
            if *count.lock().unwrap() > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::fs::write(
            home_for_clear.join("config.toml"),
            format!("[endpoints]\nmanaged_config_url = \"{url}\"\n"),
        )
        .unwrap();
    });

    let synced = xai_grok_shell::managed_config::sync()
        .await
        .expect("sync should succeed (skip, not error)");
    clearer.join().unwrap();

    assert!(!synced, "credential-gone apply must not report a write");
    assert!(
        !auths.lock().unwrap().is_empty(),
        "the fetch must have reached the server"
    );
    assert!(
        !home
            .join(xai_grok_config::MANAGED_CONFIG_CACHE_FILE)
            .exists(),
        "credential-gone must not write a marker for an unapplied body"
    );
    assert!(!home.join("requirements.toml").exists());
}

/// A dk-synced marker means the KEY owns this machine's policy: with the key line gone
/// from config.toml (the shape of a transient read failure) and a team user signed in,
/// the gate must NOT purge. Pins the marker-scoped exemption — one keyed on live config
/// resolution would purge here.
#[tokio::test]
#[serial]
async fn dk_synced_marker_survives_config_blip_with_team_signed_in() {
    let home = test_home().clone();
    reset(&home);

    let body = serde_json::json!({
        "deployment_id": "deploy-A",
        "managed_config": TEAM_MANAGED,
        "requirements": format!("fail_closed = true\n{TEAM_REQUIREMENTS}"),
    })
    .to_string();
    let (url, _auths) = spawn_mock(body);
    std::fs::write(
        home.join("config.toml"),
        format!("[endpoints]\nmanaged_config_url = \"{url}\"\ndeployment_key = \"KEY-AAA\"\n"),
    )
    .unwrap();
    xai_grok_shell::managed_config::sync()
        .await
        .expect("deploy-key sync should succeed");
    assert!(home.join("requirements.toml").exists());

    // The blip: the key line is gone (same shape as a transient config read failure),
    // while a team user is also signed in. Identity resolves Team("team-b"), which
    // differs from the marker principal ("deploy-A") — but the marker is key-scoped.
    write_config(&home, &url);
    write_team_auth(&home, "team-b");
    assert_eq!(
        xai_grok_config::confirmed_team_switch("team-b"),
        None,
        "a key-scoped marker must never confirm a team switch"
    );
    assert!(
        xai_grok_shell::managed_config::managed_policy_gate().is_ok(),
        "the blip must not refuse: the key-scoped marker still matches the on-disk policy"
    );
    assert!(
        home.join("requirements.toml").exists(),
        "the machine's enforced policy must survive the blip"
    );
    assert!(
        home.join(xai_grok_config::MANAGED_CONFIG_CACHE_FILE)
            .exists(),
        "the dk marker must survive the blip"
    );
}
