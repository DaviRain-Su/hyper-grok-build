use super::*;
use crate::session::storage::jsonl::AppendDurability;

struct ActorGuard {
    handle: PersistenceHandle,
    task: tokio::task::JoinHandle<()>,
}

impl ActorGuard {
    async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

fn test_actor(info: Info, storage: Arc<dyn StorageAdapter>) -> ActorGuard {
    test_actor_with_remote_sync(info, storage, None)
}

fn test_actor_with_remote_sync(
    info: Info,
    storage: Arc<dyn StorageAdapter>,
    remote_sync: Option<RemoteSync>,
) -> ActorGuard {
    let (tx, rx) = mpsc::unbounded_channel();
    let summary_tx = tx.clone();
    let sampling_client = OaiCompatClient::new(xai_grok_sampler::SamplerConfig::default()).unwrap();
    let task = tokio::spawn(
        SessionPersistence {
            info,
            storage,
            pending_notification: None,
            rx,
            remote_sync,
            relay_sync: None,
            summary: crate::session::summary::SummaryGenerator::new(
                crate::session::summary::SummaryConfig {
                    sampling_client,
                    model: String::new(),
                    persistence_tx: summary_tx,
                },
            ),
            registry_title_sync: None,
            gateway: None,
        }
        .run(),
    );
    ActorGuard {
        handle: PersistenceHandle { tx, noop: false },
        task,
    }
}

fn notification(info: &Info, text: &str) -> acp::SessionNotification {
    acp::SessionNotification::new(
        info.id.clone(),
        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
            acp::TextContent::new(text),
        ))),
    )
}

fn neutral_update(info: &Info, text: &str) -> SessionUpdate {
    SessionUpdate::Acp(Box::new(notification(info, text)))
}

fn break_summary_writes(dir: &std::path::Path) {
    let summary = dir.join("summary.json");
    std::fs::remove_file(&summary).unwrap();
    std::fs::create_dir(summary).unwrap();
}

async fn recv_observed(
    observed: &mut tokio::sync::mpsc::UnboundedReceiver<acp::SessionNotification>,
) -> acp::SessionNotification {
    tokio::time::timeout(std::time::Duration::from_secs(1), observed.recv())
        .await
        .expect("remote sync timed out")
        .expect("remote sync observer closed")
}

#[test]
fn committed_error_returns_sync_disposition() {
    let info = Info {
        id: acp::SessionId::new("committed-update"),
        cwd: "/test".into(),
    };
    let notification = notification(&info, "committed");
    let PendingAppendOutcome::CommittedErr(sync_notification, error) =
        SessionPersistence::finish_pending_append(
            notification,
            Err(crate::session::storage::AppendUpdateError::Committed(
                io::Error::other("summary patch failed"),
            )),
        )
    else {
        panic!("expected committed failure");
    };
    assert_eq!(sync_notification.session_id, info.id);
    assert_eq!(error.to_string(), "summary patch failed");
}

#[test]
fn uncommitted_error_returns_restore_disposition() {
    let info = Info {
        id: acp::SessionId::new("uncommitted-update"),
        cwd: "/test".into(),
    };
    let notification = notification(&info, "pending");
    let PendingAppendOutcome::NotCommittedErr(pending_notification, error) =
        SessionPersistence::finish_pending_append(
            notification,
            Err(crate::session::storage::AppendUpdateError::NotCommitted(
                io::Error::other("append failed"),
            )),
        )
    else {
        panic!("expected uncommitted failure");
    };
    assert_eq!(pending_notification.session_id, info.id);
    assert_eq!(error.to_string(), "append failed");
}

#[tokio::test]
async fn noop_handle_rejects_durable_append() {
    let info = Info {
        id: acp::SessionId::new("noop-durable-update"),
        cwd: "/test".into(),
    };
    assert!(matches!(
        PersistenceHandle::noop()
            .append_update_durably(neutral_update(&info, "durable"))
            .await,
        Err(DurableAppendError::NotCommitted(error))
            if error.kind() == io::ErrorKind::Unsupported
    ));
}

#[tokio::test]
async fn pending_drain_disposition_controls_remote_sync() {
    let info = Info {
        id: acp::SessionId::new("pending-remote-sync"),
        cwd: "/test".into(),
    };
    let storage = JsonlStorageAdapter::with_update_append_probe("/unused".into(), |_| {
        Err(io::Error::other("append failed"))
    });
    let (remote_sync, mut observed) = RemoteSync::test_observer();
    let actor = test_actor_with_remote_sync(info.clone(), Arc::new(storage), Some(remote_sync));
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "pending")))
        .unwrap();
    assert!(matches!(
        actor
            .handle
            .append_update_durably(neutral_update(&info, "durable"))
            .await,
        Err(DurableAppendError::NotCommitted(_))
    ));
    assert!(observed.try_recv().is_err());
    actor.stop().await;

    let dir = tempfile::tempdir().unwrap();
    let attempts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed_attempts = attempts.clone();
    let storage = Arc::new(JsonlStorageAdapter::with_update_append_probe(
        dir.path().to_path_buf(),
        move |durability| {
            observed_attempts.lock().unwrap().push(durability);
            Ok(())
        },
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let (remote_sync, mut observed) = RemoteSync::test_observer();
    let actor = test_actor_with_remote_sync(info.clone(), storage, Some(remote_sync));
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "pending")))
        .unwrap();
    break_summary_writes(dir.path());
    assert!(matches!(
        actor
            .handle
            .append_update_durably(neutral_update(&info, "durable"))
            .await,
        Err(DurableAppendError::Committed(_))
    ));
    let synced = recv_observed(&mut observed).await;
    assert_eq!(synced.session_id, info.id);
    assert!(matches!(
        attempts.lock().unwrap().as_slice(),
        [AppendDurability::Buffered]
    ));
    actor.stop().await;
}

#[tokio::test]
async fn durable_append_committed_failure_is_synced() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("durable-remote-sync"),
        cwd: "/test".into(),
    };
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    break_summary_writes(dir.path());
    let (remote_sync, mut observed) = RemoteSync::test_observer();
    let actor = test_actor_with_remote_sync(info.clone(), storage, Some(remote_sync));
    assert!(matches!(
        actor
            .handle
            .append_update_durably(neutral_update(&info, "durable"))
            .await,
        Err(DurableAppendError::Committed(_))
    ));
    let synced = recv_observed(&mut observed).await;
    assert_eq!(synced.session_id, info.id);
    actor.stop().await;
}

#[tokio::test]
async fn failed_pending_drain_retains_record_and_skips_durable_update() {
    let info = Info {
        id: acp::SessionId::new("durable-drain-failure"),
        cwd: "/test".into(),
    };
    let attempts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = attempts.clone();
    let storage =
        JsonlStorageAdapter::with_update_append_probe("/unused".into(), move |durability| {
            observed.lock().unwrap().push(durability);
            Err(io::Error::other("pending append failed"))
        });
    let actor = test_actor(info.clone(), Arc::new(storage));
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "pending")))
        .unwrap();
    for _ in 0..2 {
        assert_eq!(
            actor
                .handle
                .append_update_durably(neutral_update(&info, "durable"))
                .await
                .unwrap_err()
                .to_string(),
            "pending append failed"
        );
    }
    assert!(matches!(
        attempts.lock().unwrap().as_slice(),
        [AppendDurability::Buffered, AppendDurability::Buffered]
    ));
    actor.stop().await;
}

#[tokio::test]
async fn durable_append_drains_pending_update_in_fifo_order() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("durable-update"),
        cwd: dir.path().to_string_lossy().into_owned(),
    };
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let actor = test_actor(info.clone(), storage.clone());
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "before")))
        .unwrap();
    actor
        .handle
        .append_update_durably(neutral_update(&info, "durable"))
        .await
        .unwrap();
    let summary = storage.load_summary(&info).await.unwrap();
    assert_eq!(summary.num_messages, 2);

    let updates = storage.load_session(&info).await.unwrap().updates;
    let texts = updates
        .iter()
        .filter_map(|update| {
            let SessionUpdate::Acp(notification) = update else {
                return None;
            };
            let acp::SessionUpdate::AgentMessageChunk(chunk) = &notification.update else {
                return None;
            };
            let acp::ContentBlock::Text(text) = &chunk.content else {
                return None;
            };
            Some(text.text.clone())
        })
        .collect::<Vec<_>>();
    assert_eq!(texts, ["before", "durable"]);
    actor.stop().await;
}
