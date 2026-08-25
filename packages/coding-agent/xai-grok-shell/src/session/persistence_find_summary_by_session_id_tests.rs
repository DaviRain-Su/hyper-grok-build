use super::{
    SessionKindIndex, Summary, authoritative_summaries_in_root, find_summary_by_session_id_in_root,
};
use crate::session::info::Info;
use crate::session::storage::relocation::{RelocationRequest, RelocationStorage};
use crate::session::visibility::ClassifiedSessionKind;
use std::fs;
use tempfile::TempDir;

fn write_summary(root: &std::path::Path, cwd_dir: &str, session_id: &str, json: &str) {
    let dir = root.join(cwd_dir).join(session_id);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("summary.json"), json).unwrap();
}

fn minimal_summary(head_commit: &str, head_branch: &str) -> String {
    serde_json::json!({
        "info": { "id": "test-session", "cwd": "/tmp" },
        "session_summary": "",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
        "num_messages": 0,
        "current_model_id": "grok-3",
        "head_commit": head_commit,
        "head_branch": head_branch
    })
    .to_string()
}

#[test]
fn returns_none_when_root_missing() {
    let result =
        find_summary_by_session_id_in_root("any", &std::path::PathBuf::from("/nonexistent"));
    assert!(result.is_none());
}

#[test]
fn returns_none_when_no_matching_session() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("sessions");
    write_summary(&root, "cwd1", "other-id", &minimal_summary("abc", "main"));
    assert!(find_summary_by_session_id_in_root("missing-id", &root).is_none());
}

#[test]
fn finds_summary_across_cwd_dirs() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("sessions");
    write_summary(
        &root,
        "encoded_cwd",
        "target-session",
        &minimal_summary("deadbeef", "feature/x"),
    );

    let found = find_summary_by_session_id_in_root("target-session", &root).unwrap();
    assert_eq!(found.head_commit.as_deref(), Some("deadbeef"));
    assert_eq!(found.head_branch.as_deref(), Some("feature/x"));
}

#[test]
fn skips_malformed_summary() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("sessions");
    // Write invalid JSON
    let dir = root.join("cwd1").join("bad-session");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("summary.json"), b"not-json").unwrap();

    assert!(find_summary_by_session_id_in_root("bad-session", &root).is_none());
}

#[test]
fn authoritative_scan_uses_relocated_target_once() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("sessions");
    let storage = RelocationStorage::new(tmp.path().into());
    let info = Info {
        id: agent_client_protocol::SessionId::new("moved"),
        cwd: "/source".into(),
    };
    let dir = root
        .join(crate::util::grok_home::encode_cwd_dirname(&info.cwd))
        .join(info.id.to_string());
    fs::create_dir_all(&dir).unwrap();
    let mut summary = Summary::new(&info, agent_client_protocol::ModelId::new("model")).unwrap();
    summary.session_kind = Some("headless".into());
    fs::write(
        dir.join("summary.json"),
        serde_json::to_vec(&summary).unwrap(),
    )
    .unwrap();

    let lease = storage.acquire("moved").unwrap();
    let staged = storage
        .stage_and_publish(
            &lease,
            RelocationRequest {
                session_id: "moved".into(),
                nonce: "test".into(),
                source_cwd: "/source".into(),
                target_cwd: "/target".into(),
                cwd_generation: 1,
                pending_reminder: crate::session::persistence::PendingCwdSwitchReminder {
                    cwd_generation: 1,
                    previous_cwd: "/source".into(),
                    destination_cwd: "/target".into(),
                    content: "moved".into(),
                    destination_project_instructions: None,
                },
            },
        )
        .unwrap();
    let _ = storage.mark_ready_and_commit(&lease, &staged).unwrap();

    let rows = authoritative_summaries_in_root(&root).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].info.cwd, "/target");
    assert!(rows[0].is_headless());
}

#[test]
fn kind_index_classifies_requested_ids_without_requiring_a_full_scan() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("sessions");
    write_summary(
        &root,
        "cwd1",
        "interactive",
        &minimal_summary("abc", "main"),
    );
    write_summary(
        &root,
        "cwd1",
        "headless-one",
        &serde_json::json!({
            "info": { "id": "headless-one", "cwd": "/tmp" },
            "session_summary": "",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "num_messages": 0,
            "current_model_id": "grok-3",
            "session_kind": "headless"
        })
        .to_string(),
    );
    let dir = root.join("cwd1").join("bad-session");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("summary.json"), b"not-json").unwrap();

    let index = SessionKindIndex::load_in_root(&root).unwrap();
    assert_eq!(
        index.kind("interactive"),
        ClassifiedSessionKind::Interactive
    );
    assert_eq!(index.kind("headless-one"), ClassifiedSessionKind::Headless);
    assert_eq!(index.kind("bad-session"), ClassifiedSessionKind::Unknown);
    assert_eq!(index.kind("missing"), ClassifiedSessionKind::Unknown);
}
