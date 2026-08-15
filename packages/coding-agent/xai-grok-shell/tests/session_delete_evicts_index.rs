//! One binary, one home: `grok_home()` memoizes the first read for the process, so tests that
//! need a temp home have to share one, and `#[serial]` keeps their env writes apart.

use std::sync::{Arc, OnceLock};

use agent_client_protocol as acp;
use xai_grok_shell::auth::{AuthManager, GrokComConfig};
use xai_grok_shell::session::info::Info;
use xai_grok_shell::session::persistence::delete_session_history;
use xai_grok_shell::session::storage::search::{SessionSearchRequest, execute_search};
use xai_grok_shell::session::storage::{JsonlStorageAdapter, StorageAdapter};
use xai_grok_test_support::EnvGuard;

fn home() -> &'static std::path::Path {
    static HOME: OnceLock<(tempfile::TempDir, EnvGuard)> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = tempfile::TempDir::new().unwrap();
        let guard = EnvGuard::set("GROK_HOME", dir.path());
        (dir, guard)
    })
    .0
    .path()
}

/// Titles are one made-up token, searched back verbatim: a query of ordinary words ORs its
/// tokens, which would let one session's row answer for another.
async fn seed_session(root: &std::path::Path, id: &str, cwd: &str) {
    let storage = JsonlStorageAdapter::with_root(root.to_path_buf());
    let info = Info {
        id: acp::SessionId::new(id),
        cwd: cwd.to_string(),
    };
    storage
        .init_session(&info, acp::ModelId::new("test-model"))
        .await
        .unwrap();
    storage
        .update_session_title(&info, title_for(id))
        .await
        .unwrap();
}

fn title_for(id: &str) -> String {
    format!("zzqq{id}")
}

async fn finds(root: &std::path::Path, id: &str) -> bool {
    let req = SessionSearchRequest {
        query: title_for(id),
        cwd: None,
        limit: 10,
        offset: 0,
        include_content: false,
    };
    !execute_search(root, &req).await.unwrap().results.is_empty()
}

#[tokio::test]
#[serial_test::serial]
async fn deleting_a_session_clears_only_its_own_search_row() {
    let root = home();

    // All three up front: only the first search bootstraps.
    seed_session(root, "orphan", "/ws-a").await;
    seed_session(root, "elsewhere", "/ws-b").await;
    seed_session(root, "scoped", "/ws-c").await;
    assert!(finds(root, "orphan").await, "precondition: indexed");
    assert!(finds(root, "elsewhere").await, "precondition: indexed");
    assert!(finds(root, "scoped").await, "precondition: indexed");

    let auth = Arc::new(AuthManager::new(root, GrokComConfig::default()));

    let session_dir =
        xai_grok_shell::util::grok_home::sessions_cwd_dir_in(root, "/ws-a").join("orphan");
    std::fs::remove_dir_all(&session_dir).unwrap();
    let deletion = delete_session_history("orphan", None, false, auth.clone())
        .await
        .unwrap();
    assert!(!deletion.any_removed(), "nothing was left to remove");
    assert!(
        !finds(root, "orphan").await,
        "a delete with no workspace must clear a row nothing else will ever prune",
    );

    delete_session_history("elsewhere", Some("/ws-a"), false, auth.clone())
        .await
        .unwrap();
    assert!(
        finds(root, "elsewhere").await,
        "a delete scoped to another workspace must not evict this session",
    );

    let deletion = delete_session_history("scoped", Some("/ws-c"), false, auth)
        .await
        .unwrap();
    assert!(deletion.local_removed, "the session was there to remove");
    assert!(
        !finds(root, "scoped").await,
        "a delete that removed the session must take its row too",
    );
}

#[test]
#[serial_test::serial]
fn loading_config_applies_requirement_pins() {
    // Removed again before returning: the other test resolves its config from this same home.
    let pin = home().join("requirements.toml");
    std::fs::write(&pin, "[features]\nsession_search = false\n").unwrap();

    let loaded = xai_grok_shell::config::load_agent_config_disk_only();
    std::fs::remove_file(&pin).unwrap();
    let config = loaded.expect("config loads");

    assert_eq!(
        config.requirements.session_search.pinned(),
        Some(false),
        "a one-shot command must apply pins, or the environment outranks them",
    );
}
