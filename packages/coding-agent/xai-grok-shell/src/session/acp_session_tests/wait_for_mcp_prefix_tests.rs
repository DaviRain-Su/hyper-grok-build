use super::support::*;
use super::*;
use xai_grok_agent::prompt::user_message::UserMessageTemplate;
/// Builds a test actor whose `mcp_state` holds `configs`, then drives the typed init transitions to match `(initialized, initializing_servers)`.
///
/// Mapping:
/// - `initialized=false`, no servers: `InitProgress::NotStarted`
/// - `initialized=false`, some servers: `Starting`, with those servers handshaking
/// - `initialized=true`, no servers: `Finished`, with no handshakes pending
/// - `initialized=true`, some servers: `Finished` early, with those handshakes still running in the background
async fn actor_with_mcp(
    configs: Vec<acp::McpServer>,
    initialized: bool,
    initializing_servers: Vec<String>,
) -> SessionActor {
    let (gw_tx, _gw_rx) = tokio::sync::mpsc::unbounded_channel();
    let (persist_tx, _persist_rx) = tokio::sync::mpsc::unbounded_channel();
    let actor = create_test_actor(100, 256_000, 80, gw_tx, persist_tx).await;
    {
        let mut state = actor.mcp_state.lock().await;
        state.configs = configs;
        state.cancel_init();
        if initialized || !initializing_servers.is_empty() {
            assert!(state.try_start_init());
            state.mark_servers_initializing(initializing_servers);
            if initialized {
                state.finish_init();
            }
        }
    }
    actor
}
fn dummy_stdio_config(name: &str) -> acp::McpServer {
    acp::McpServer::Stdio(
        acp::McpServerStdio::new(name.to_string(), "true")
            .args(vec![])
            .env(vec![]),
    )
}
#[tokio::test(flavor = "current_thread")]
async fn returns_immediately_for_default_template() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = actor_with_mcp(
                vec![dummy_stdio_config("linear")],
                false,
                vec!["linear".into()],
            )
            .await;
            let start = std::time::Instant::now();
            actor
                .wait_for_mcp_templated_prefix_ready(&UserMessageTemplate::Default)
                .await;
            assert!(start.elapsed() < std::time::Duration::from_millis(50));
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn wait_for_mcp_initialized_parks_until_a_foreign_init_completes() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = actor_with_mcp(
                vec![dummy_stdio_config("linear")],
                false,
                vec!["linear".into()],
            )
            .await;
            let wait = actor.wait_for_mcp_initialized();
            tokio::pin!(wait);
            tokio::select! {
                _ = &mut wait => panic!("returned while the foreign init was still in flight"),
                _ = tokio::time::sleep(std::time::Duration::from_millis(60)) => {}
            }
            {
                let mut state = actor.mcp_state.lock().await;
                state.finish_init();
                state.mark_all_servers_ready();
            }
            tokio::time::timeout(std::time::Duration::from_secs(2), wait)
                .await
                .expect("must resolve once the foreign init completes");
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn resolved_repo_status_prefetch_builds_first_prefix_with_zero_wait() {
    use crate::session::repo_status_prefix::{
        RepoStatusInputs, RepoStatusPlan, RepoStatusPrefetch, RepoStatusPrefetchState,
        RepoStatusSnapshot,
    };
    use xai_grok_workspace::session::git::VcsKind;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gw_tx, _gw_rx) = tokio::sync::mpsc::unbounded_channel();
            let (persist_tx, _persist_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut actor = create_test_actor(100, 256_000, 80, gw_tx, persist_tx).await;
            let (snapshot_tx, snapshot_rx) = tokio::sync::watch::channel(None);
            snapshot_tx
                .send(Some(RepoStatusSnapshot {
                    root: None,
                    raw_status: Some("## main\n M src/lib.rs\n".to_string()),
                    vcs_kind: VcsKind::Git,
                }))
                .unwrap();
            actor.repo_status_prefetch = RepoStatusPrefetchState::new(RepoStatusPlan::Gather {
                inputs: RepoStatusInputs {
                    cwd: std::path::PathBuf::from("."),
                    vcs_kind: VcsKind::Git,
                    root: None,
                },
                prefetch: std::cell::RefCell::new(Some(RepoStatusPrefetch::from_snapshot_rx(
                    snapshot_rx,
                ))),
            });
            let prefix = actor.build_user_message_prefix().await;
            assert!(
                prefix.contains("<git_status>") && prefix.contains(" M src/lib.rs"),
                "prefix must render the prefetched status, got: {prefix}"
            );
            assert!(
                actor
                    .repo_status_prefetch
                    .take_wait_ms()
                    .is_some_and(|ms| ms < 100),
                "resolved prefetch must not consume the wait budget"
            );
            assert!(
                matches!(
                    actor.repo_status_prefetch.plan(),
                    RepoStatusPlan::Gather { prefetch, .. } if prefetch.borrow().is_none()
                ),
                "prefetch handle is consumed exactly once"
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn absent_inputs_omit_the_status_block() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gw_tx, _gw_rx) = tokio::sync::mpsc::unbounded_channel();
            let (persist_tx, _persist_rx) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(100, 256_000, 80, gw_tx, persist_tx).await;
            let prefix = actor.build_user_message_prefix().await;
            assert!(
                !prefix.contains("<git_status>") && !prefix.contains("<jj_status>"),
                "absent inputs must omit the status block, got: {prefix}"
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn resumed_session_gathers_status_inline_without_a_prefetch() {
    use crate::session::repo_status_prefix::{
        RepoStatusInputs, RepoStatusPlan, RepoStatusPrefetchState,
    };
    use xai_grok_workspace::session::git::VcsKind;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            crate::test_support::ensure_hermetic_git_on_path();
            let repo = tempfile::tempdir().unwrap();
            let init = std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(repo.path())
                .status()
                .expect("spawn git init");
            assert!(init.success(), "git init failed");
            std::fs::write(repo.path().join("untracked.txt"), b"x").unwrap();
            let (gw_tx, _gw_rx) = tokio::sync::mpsc::unbounded_channel();
            let (persist_tx, _persist_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut actor = create_test_actor(100, 256_000, 80, gw_tx, persist_tx).await;
            actor.repo_status_prefetch = RepoStatusPrefetchState::new(RepoStatusPlan::Gather {
                inputs: RepoStatusInputs {
                    cwd: repo.path().to_path_buf(),
                    vcs_kind: VcsKind::Git,
                    root: Some(repo.path().to_path_buf()),
                },
                prefetch: std::cell::RefCell::new(None),
            });
            let prefix = actor.build_user_message_prefix().await;
            assert!(
                prefix.contains("<git_status>") && prefix.contains("untracked.txt"),
                "inline gather (no prefetch) must render the status block, got: {prefix}"
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn suppressed_status_omits_the_body_but_keeps_the_repo_root() {
    use crate::session::repo_status_prefix::{
        RepoStatusPlan, RepoStatusPrefetchState, discover_vcs_root,
    };
    use xai_grok_workspace::session::git::VcsKind;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            crate::test_support::ensure_hermetic_git_on_path();
            let repo = tempfile::tempdir().unwrap();
            let init = std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(repo.path())
                .status()
                .expect("spawn git init");
            assert!(init.success(), "git init failed");
            let (gw_tx, _gw_rx) = tokio::sync::mpsc::unbounded_channel();
            let (persist_tx, _persist_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut actor = create_test_actor(100, 256_000, 80, gw_tx, persist_tx).await;
            actor.repo_status_prefetch = RepoStatusPrefetchState::new(RepoStatusPlan::RootOnly {
                root: discover_vcs_root(repo.path()),
                vcs_kind: VcsKind::Git,
            });
            let prefix = actor.build_user_message_prefix().await;
            assert!(
                !prefix.contains("<git_status>"),
                "suppressed status must omit the body, got: {prefix}"
            );
            assert!(
                matches!(
                    actor.repo_status_prefetch.plan(),
                    RepoStatusPlan::RootOnly { root: Some(_), .. }
                ),
                "suppressed status must keep the discovered repo root"
            );
        })
        .await;
}
