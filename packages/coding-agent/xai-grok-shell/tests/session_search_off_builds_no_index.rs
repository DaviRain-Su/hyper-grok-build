//! The switch reaching the index through the shell rather than injected into the crate. An
//! unresolved latch resolves the config itself, so this covers that path, not the `initialize`
//! call that normally applies the gate.
//! Its own binary: the home and the latch both resolve once per process.

use agent_client_protocol as acp;
use xai_grok_shell::session::info::Info;
use xai_grok_shell::session::storage::search::{
    SessionSearchRequest, execute_search, notify_session_updated,
};
use xai_grok_shell::session::storage::{JsonlStorageAdapter, StorageAdapter};
use xai_grok_test_support::EnvGuard;

#[tokio::test]
async fn a_saved_session_is_neither_indexed_nor_found_with_search_off() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    let _home = EnvGuard::set("GROK_HOME", root);
    let _off = EnvGuard::set("GROK_SESSION_SEARCH", "0");

    let info = Info {
        id: acp::SessionId::new("s1"),
        cwd: "/ws".to_string(),
    };
    let storage = JsonlStorageAdapter::with_root(root.to_path_buf());
    storage
        .init_session(&info, acp::ModelId::new("test-model"))
        .await
        .unwrap();
    storage
        .update_session_title(&info, "zzqqtitle".to_string())
        .await
        .unwrap();
    notify_session_updated("s1", "/ws");

    let resp = execute_search(
        root,
        &SessionSearchRequest {
            query: "zzqqtitle".to_string(),
            cwd: None,
            limit: 10,
            offset: 0,
            include_content: false,
        },
    )
    .await
    .unwrap();

    assert!(resp.results.is_empty(), "a search must find nothing");
    // Prefix, not the plain name: on a network home the journal mode picks a per-host sibling,
    // which is why the operator doc says to delete `session_search*`.
    let index_files: Vec<String> = std::fs::read_dir(root.join("sessions"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("session_search"))
        .collect();
    assert!(
        index_files.is_empty(),
        "the switch is off, so no index may be built, found {index_files:?}",
    );
}
