use super::*;
use crate::session::info::Info;
use crate::session::persistence::default_model_id;
use crate::session::storage::{SessionUpdate, StorageAdapter};

fn info() -> Info {
    Info {
        id: acp::SessionId::new("durable-jsonl"),
        cwd: "/test".into(),
    }
}

fn update(info: &Info, text: String) -> SessionUpdate {
    SessionUpdate::Acp(Box::new(acp::SessionNotification::new(
        info.id.clone(),
        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
            acp::TextContent::new(text),
        ))),
    )))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordinary_and_durable_appends_keep_every_physical_line_parseable() {
    const N: usize = 100;
    let dir = tempfile::tempdir().unwrap();
    let info = info();
    let adapter = JsonlStorageAdapter::with_explicit_session_dir(dir.path().to_path_buf());
    adapter
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let ordinary = adapter.clone();
    let durable = adapter.clone();
    let info_a = info.clone();
    let info_b = info.clone();
    let ordinary = tokio::spawn(async move {
        for index in 0..N {
            ordinary
                .append_update(&info_a, &update(&info_a, format!("ordinary-{index}")))
                .await
                .unwrap();
        }
    });
    let durable = tokio::spawn(async move {
        for index in 0..N {
            durable
                .append_update_durable_commit_aware(
                    &info_b,
                    &update(&info_b, format!("durable-{index}")),
                )
                .await
                .unwrap();
        }
    });
    ordinary.await.unwrap();
    durable.await.unwrap();

    let bytes = std::fs::read(dir.path().join("updates.jsonl")).unwrap();
    let parsed = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(serde_json::from_slice::<SessionUpdateEnvelope>)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(parsed.len(), N * 2);
}

#[tokio::test]
async fn append_commit_is_reported_when_bookkeeping_fails() {
    let dir = tempfile::tempdir().unwrap();
    let info = info();
    let adapter = JsonlStorageAdapter::with_explicit_session_dir(dir.path().to_path_buf());
    adapter
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let summary = dir.path().join("summary.json");
    std::fs::remove_file(&summary).unwrap();
    std::fs::create_dir(&summary).unwrap();

    assert!(matches!(
        adapter
            .append_update_durable_commit_aware(&info, &update(&info, "committed".into()))
            .await,
        Err(crate::session::storage::AppendUpdateError::Committed(_))
    ));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("updates.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn directory_barrier_failure_is_retried_even_after_file_exists() {
    let mut attempts = 0;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("updates.jsonl");
    let mut flaky_parent = || {
        attempts += 1;
        if attempts == 1 {
            Err(io::Error::other("directory barrier failed"))
        } else {
            Ok(())
        }
    };
    assert!(
        JsonlStorageAdapter::append_jsonl_line_sync_with(
            &path,
            b"{\"record\":1}\n".to_vec(),
            AppendDurability::Durable,
            std::fs::File::sync_all,
            &mut flaky_parent,
        )
        .is_err()
    );
    JsonlStorageAdapter::append_jsonl_line_sync_with(
        &path,
        b"{\"record\":1}\n".to_vec(),
        AppendDurability::Durable,
        std::fs::File::sync_all,
        &mut flaky_parent,
    )
    .unwrap();
    assert_eq!(attempts, 2);
}
