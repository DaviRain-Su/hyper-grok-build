//! Dashboard v2 workspace adoption and metadata synchronization.
use super::actions::Effect;
use super::agent_view::AgentView;
use super::app_view::AppView;
use std::time::UNIX_EPOCH;
use xai_grok_dashboard_store::{
    MAX_CWD_BYTES, MAX_MODEL_BYTES, MAX_SUMMARY_BYTES, MAX_TITLE_BYTES, Member, MemberKind,
    MemberMetadata, MemberOrigin, NewMember, SessionId, WORKSPACE_CAPACITY,
};
/// Note that agent state may need mirroring into the initialized workspace.
pub(crate) fn request(app: &mut AppView) {
    if app.workspace_dashboard_enabled
        && (app.workspace_snapshot.is_some()
            || app.workspace_store_loading
            || app.workspace_write_in_flight)
    {
        app.workspace_sync_requested = true;
    }
}
/// Build and start one serialized workspace write, if current agent metadata
/// differs from the cached snapshot.
pub(crate) fn drain(app: &mut AppView) -> Vec<Effect> {
    if !app.workspace_sync_requested {
        return vec![];
    }
    if !app.workspace_dashboard_enabled || app.workspace_writes_disabled {
        app.workspace_sync_requested = false;
        return vec![];
    }
    let Some(snapshot) = app.workspace_snapshot.as_ref() else {
        if !app.workspace_store_loading {
            app.workspace_sync_requested = false;
        }
        return vec![];
    };
    if app.workspace_store.is_none() {
        return vec![];
    }
    let mut seen = std::collections::HashSet::new();
    let candidates: Vec<_> = app
        .agents
        .values()
        .filter_map(agent_to_new_member)
        .filter(|candidate| seen.insert(candidate.key.session_id.clone()))
        .collect();
    let existing_ids: std::collections::HashSet<_> = snapshot
        .members
        .iter()
        .filter(|member| matches!(member.kind, MemberKind::Build))
        .map(|member| member.session_id.clone())
        .collect();
    let candidate_ids: std::collections::HashSet<_> = candidates
        .iter()
        .map(|candidate| candidate.key.session_id.clone())
        .collect();
    let pinned_non_candidates = snapshot
        .members
        .iter()
        .filter(|member| {
            member.pin_rank.is_some()
                && !(matches!(member.kind, MemberKind::Build)
                    && candidate_ids.contains(&member.session_id))
        })
        .count();
    let mut existing = Vec::new();
    let mut missing = Vec::new();
    for candidate in candidates {
        if existing_ids.contains(&candidate.key.session_id) {
            existing.push(candidate);
        } else {
            missing.push(candidate);
        }
    }
    let missing_slots = WORKSPACE_CAPACITY
        .saturating_sub(pinned_non_candidates)
        .saturating_sub(existing.len());
    existing.extend(missing.into_iter().take(missing_slots));
    let mut members = Vec::new();
    for candidate in existing {
        let existing = snapshot.members.iter().find(|member| {
            matches!(member.kind, MemberKind::Build)
                && member.session_id == candidate.key.session_id
        });
        if existing.is_some_and(|member| metadata_matches(member, &candidate.metadata)) {
            continue;
        }
        if app
            .workspace_failed_metadata
            .get(&candidate.key.session_id)
            .is_some_and(|metadata| metadata == &candidate.metadata)
        {
            continue;
        }
        members.push(candidate);
    }
    app.workspace_sync_requested = false;
    if members.is_empty() {
        return vec![];
    }
    let Some(store) = app.workspace_store.take() else {
        return vec![];
    };
    app.workspace_write_in_flight = true;
    vec![Effect::UpsertWorkspaceMembers { store, members }]
}
fn agent_to_new_member(agent: &AgentView) -> Option<NewMember> {
    let cwd = agent.session.cwd.to_string_lossy();
    if agent.conversation_entry || !agent.session.cwd.is_absolute() || cwd.len() > MAX_CWD_BYTES {
        return None;
    }
    let session_id = SessionId::new(agent.session.session_id.as_ref()?.0.to_string()).ok()?;
    Some(NewMember {
        key: xai_grok_dashboard_store::MemberKey {
            session_id,
            kind: MemberKind::Build,
        },
        origin: MemberOrigin::Local,
        metadata: agent_metadata(agent),
    })
}
fn agent_metadata(agent: &AgentView) -> MemberMetadata {
    let state = crate::views::dashboard::classify_top_level(agent);
    let last_change = crate::views::dashboard::row::top_level_last_change_at(agent, state);
    let last_change_unix_ms = last_change
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_mul(1_000)
        .try_into()
        .unwrap_or(i64::MAX);
    MemberMetadata {
        cwd: Some(agent.session.cwd.display().to_string()),
        title: truncate(
            crate::views::session_title::rename_source_title(agent),
            MAX_TITLE_BYTES,
        ),
        model: truncate(
            agent
                .session
                .models
                .current_model_id_str()
                .map(str::to_owned),
            MAX_MODEL_BYTES,
        ),
        last_turn_summary: truncate(agent.last_turn_summary.clone(), MAX_SUMMARY_BYTES),
        is_worktree: agent.is_worktree || agent.session.is_worktree,
        last_change_unix_ms,
    }
}
fn truncate(value: Option<String>, max_bytes: usize) -> Option<String> {
    value.map(|mut value| {
        if value.len() > max_bytes {
            let mut end = max_bytes;
            while !value.is_char_boundary(end) {
                end -= 1;
            }
            value.truncate(end);
        }
        value
    })
}
fn metadata_matches(member: &Member, metadata: &MemberMetadata) -> bool {
    member.cwd == metadata.cwd
        && member.title == metadata.title
        && member.model == metadata.model
        && member.last_turn_summary == metadata.last_turn_summary
        && member.is_worktree == metadata.is_worktree
        && member.last_change_unix_ms == metadata.last_change_unix_ms
}
#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol as acp;
    fn eligible_agent() -> AgentView {
        let mut agent = crate::app::agent_view::test_fixtures::make_agent();
        agent.session.session_id = Some(acp::SessionId::new("saved"));
        agent.session.cwd = "/tmp/workspace-sync".into();
        agent
    }
    #[test]
    fn maps_bound_local_build_agent_metadata() {
        let mut agent = eligible_agent();
        agent.display_name = Some("Saved title".into());
        agent.last_turn_summary = Some("Summary".into());
        agent.session.is_worktree = true;
        let member = agent_to_new_member(&agent).expect("eligible build agent");
        assert_eq!(member.key.session_id.as_ref(), "saved");
        assert!(matches!(member.key.kind, MemberKind::Build));
        assert!(matches!(member.origin, MemberOrigin::Local));
        assert_eq!(member.metadata.cwd.as_deref(), Some("/tmp/workspace-sync"));
        assert_eq!(member.metadata.title.as_deref(), Some("Saved title"));
        assert_eq!(
            member.metadata.last_turn_summary.as_deref(),
            Some("Summary")
        );
        assert!(member.metadata.is_worktree);
        assert!(member.metadata.last_change_unix_ms > 0);
    }
    #[test]
    fn skips_unbound_conversation_and_relative_cwd_agents() {
        let mut agent = crate::app::agent_view::test_fixtures::make_agent();
        agent.session.cwd = "/tmp/workspace-sync".into();
        assert!(agent_to_new_member(&agent).is_none());
        agent.session.session_id = Some(acp::SessionId::new("saved"));
        agent.conversation_entry = true;
        assert!(agent_to_new_member(&agent).is_none());
        agent.conversation_entry = false;
        agent.session.cwd = "relative".into();
        assert!(agent_to_new_member(&agent).is_none());
    }
    #[test]
    fn metadata_is_canonicalized_to_store_limits() {
        let mut agent = eligible_agent();
        agent.display_name = Some("é".repeat(MAX_TITLE_BYTES));
        agent.last_turn_summary = Some("s".repeat(MAX_SUMMARY_BYTES + 10));
        let member = agent_to_new_member(&agent).unwrap();
        let title = member.metadata.title.unwrap();
        assert!(title.len() <= MAX_TITLE_BYTES);
        assert!(title.is_char_boundary(title.len()));
        assert_eq!(
            member.metadata.last_turn_summary.unwrap().len(),
            MAX_SUMMARY_BYTES
        );
    }
    #[test]
    fn drain_moves_the_single_store_into_one_batch() {
        let temp = tempfile::tempdir().unwrap();
        let store =
            xai_grok_dashboard_store::WorkspaceStore::open(&temp.path().join("workspace.db"))
                .unwrap();
        let snapshot = store.snapshot().unwrap();
        let mut app = crate::app::app_view::tests::test_app();
        app.workspace_dashboard_enabled = true;
        app.workspace_store = Some(store);
        app.workspace_snapshot = Some(snapshot);
        app.workspace_sync_requested = true;
        app.agents
            .insert(crate::app::agent::AgentId(0), eligible_agent());
        let effects = drain(&mut app);
        assert!(matches!(
            effects.as_slice(),
            [Effect::UpsertWorkspaceMembers { members, .. }] if members.len() == 1
        ));
        assert!(app.workspace_store.is_none());
        assert!(app.workspace_write_in_flight);
        assert!(!app.workspace_sync_requested);
    }
    #[test]
    fn drain_skips_metadata_already_in_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let mut store =
            xai_grok_dashboard_store::WorkspaceStore::open(&temp.path().join("workspace.db"))
                .unwrap();
        let agent = eligible_agent();
        store
            .insert_member(agent_to_new_member(&agent).unwrap())
            .unwrap();
        let snapshot = store.snapshot().unwrap();
        let mut app = crate::app::app_view::tests::test_app();
        app.workspace_dashboard_enabled = true;
        app.workspace_store = Some(store);
        app.workspace_snapshot = Some(snapshot);
        app.workspace_sync_requested = true;
        app.agents.insert(crate::app::agent::AgentId(0), agent);
        assert!(drain(&mut app).is_empty());
        assert!(app.workspace_store.is_some());
        assert!(!app.workspace_write_in_flight);
    }
    #[test]
    fn drain_keeps_request_while_write_owns_store() {
        let temp = tempfile::tempdir().unwrap();
        let store =
            xai_grok_dashboard_store::WorkspaceStore::open(&temp.path().join("workspace.db"))
                .unwrap();
        let snapshot = store.snapshot().unwrap();
        let mut app = crate::app::app_view::tests::test_app();
        app.workspace_dashboard_enabled = true;
        app.workspace_snapshot = Some(snapshot);
        app.workspace_write_in_flight = true;
        app.workspace_sync_requested = true;
        assert!(drain(&mut app).is_empty());
        assert!(app.workspace_sync_requested);
    }
    #[test]
    fn drain_suppresses_identical_failed_metadata_until_it_changes() {
        let temp = tempfile::tempdir().unwrap();
        let store =
            xai_grok_dashboard_store::WorkspaceStore::open(&temp.path().join("workspace.db"))
                .unwrap();
        let snapshot = store.snapshot().unwrap();
        let agent = eligible_agent();
        let failed = agent_to_new_member(&agent).unwrap();
        let mut app = crate::app::app_view::tests::test_app();
        app.workspace_dashboard_enabled = true;
        app.workspace_store = Some(store);
        app.workspace_snapshot = Some(snapshot);
        app.workspace_sync_requested = true;
        app.workspace_failed_metadata
            .insert(failed.key.session_id, failed.metadata);
        app.agents.insert(crate::app::agent::AgentId(0), agent);
        assert!(drain(&mut app).is_empty());
        app.agents
            .get_mut(&crate::app::agent::AgentId(0))
            .unwrap()
            .display_name = Some("Changed".into());
        app.workspace_sync_requested = true;
        assert!(matches!(
            drain(&mut app).as_slice(),
            [Effect::UpsertWorkspaceMembers { members, .. }] if members.len() == 1
        ));
    }
    #[test]
    fn local_reinsert_preserves_existing_remote_origin() {
        let temp = tempfile::tempdir().unwrap();
        let mut store =
            xai_grok_dashboard_store::WorkspaceStore::open(&temp.path().join("workspace.db"))
                .unwrap();
        let agent = eligible_agent();
        let mut remote = agent_to_new_member(&agent).unwrap();
        remote.origin = MemberOrigin::Remote;
        store.insert_member(remote).unwrap();
        store
            .insert_member(agent_to_new_member(&agent).unwrap())
            .unwrap();
        let snapshot = store.snapshot().unwrap();
        assert!(matches!(snapshot.members[0].origin, MemberOrigin::Remote));
    }
    #[test]
    fn drain_caps_live_candidates_to_workspace_capacity() {
        let temp = tempfile::tempdir().unwrap();
        let store =
            xai_grok_dashboard_store::WorkspaceStore::open(&temp.path().join("workspace.db"))
                .unwrap();
        let snapshot = store.snapshot().unwrap();
        let mut app = crate::app::app_view::tests::test_app();
        app.workspace_dashboard_enabled = true;
        app.workspace_store = Some(store);
        app.workspace_snapshot = Some(snapshot);
        app.workspace_sync_requested = true;
        for index in 0..=WORKSPACE_CAPACITY {
            let mut agent = eligible_agent();
            agent.session.session_id = Some(acp::SessionId::new(format!("saved-{index}")));
            app.agents.insert(crate::app::agent::AgentId(index), agent);
        }
        let effects = drain(&mut app);
        assert!(matches!(
            effects.as_slice(),
            [Effect::UpsertWorkspaceMembers { members, .. }]
                if members.len() == WORKSPACE_CAPACITY
        ));
    }
    #[test]
    fn drain_reserves_capacity_for_pinned_non_candidates() {
        let temp = tempfile::tempdir().unwrap();
        let store =
            xai_grok_dashboard_store::WorkspaceStore::open(&temp.path().join("workspace.db"))
                .unwrap();
        let mut stored = Vec::new();
        for index in 0..(WORKSPACE_CAPACITY - 2) {
            stored.push(Member {
                session_id: SessionId::new(format!("pinned-{index}")).unwrap(),
                kind: MemberKind::Build,
                origin: MemberOrigin::Local,
                cwd: Some("/tmp/pinned".into()),
                title: None,
                model: None,
                last_turn_summary: None,
                is_worktree: false,
                last_change_unix_ms: 1,
                pin_rank: Some(index as i64),
                order_rank: None,
            });
        }
        stored.push(Member {
            session_id: SessionId::new("live-0").unwrap(),
            kind: MemberKind::Conversation,
            origin: MemberOrigin::Local,
            cwd: None,
            title: None,
            model: None,
            last_turn_summary: None,
            is_worktree: false,
            last_change_unix_ms: 1,
            pin_rank: Some(i64::MAX),
            order_rank: None,
        });
        let snapshot = xai_grok_dashboard_store::WorkspaceSnapshot {
            grouping: xai_grok_dashboard_store::Grouping::State,
            members: stored,
            data_version: 1,
        };
        let mut app = crate::app::app_view::tests::test_app();
        app.workspace_dashboard_enabled = true;
        app.workspace_store = Some(store);
        app.workspace_snapshot = Some(snapshot);
        app.workspace_sync_requested = true;
        for index in 0..2 {
            let mut agent = eligible_agent();
            agent.session.session_id = Some(acp::SessionId::new(format!("live-{index}")));
            app.agents.insert(crate::app::agent::AgentId(index), agent);
        }
        let effects = drain(&mut app);
        assert!(matches!(
            effects.as_slice(),
            [Effect::UpsertWorkspaceMembers { members, .. }] if members.len() == 1
        ));
    }
}
