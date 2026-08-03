#![allow(dead_code)] // Phase 1 internal helpers

use crate::permission::types::EditPolicy;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use xai_grok_paths::AbsPathBuf;
use xai_grok_tools::util::grok_home::grok_home;

const VALIDATED_MCP_SERVER_GRANTS_VERSION: i64 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionState {
    pub edit_policy: EditPolicy,
    pub allow_bash_execute: bool,
    pub allowed_bash_commands: HashSet<String>,
    pub disallowed_bash_commands: HashSet<String>,
    /// Glob patterns the user authored via the "Always allow" pattern editor
    /// (e.g. `gh api repos/owner/*`). Matched with glob semantics, unlike the
    /// literal-prefix [`Self::allowed_bash_commands`]; kept separate so a command
    /// grant that happens to contain shell metacharacters is never a wildcard.
    pub allowed_bash_globs: HashSet<String>,
    /// Domains the user has approved for `web_fetch`
    /// during this session.
    pub allowed_web_fetch_domains: HashSet<String>,
    /// Exact MCP tool names (e.g. `"grok_com_notion__notion-fetch"`)
    /// the user has granted "always allow" for. Lookup is exact.
    pub allowed_mcp_tools: HashSet<String>,
    /// Server components of valid qualified MCP IDs (e.g. `"grok_com_notion"`)
    /// for which the user has granted "always allow" to every tool. Lookup
    /// validates and parses the complete qualified ID before matching.
    pub allowed_mcp_servers: HashSet<String>,
    /// Version proving server-wide grants were minted from validated qualified IDs.
    /// Missing or malformed markers are legacy; future integer versions are preserved.
    #[serde(
        default = "legacy_mcp_server_grants_version",
        deserialize_with = "deserialize_mcp_server_grants_version"
    )]
    pub(crate) validated_mcp_server_grants_version: i64,
}

fn legacy_mcp_server_grants_version() -> i64 {
    0
}

fn deserialize_mcp_server_grants_version<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = toml::Value::deserialize(deserializer)?;
    Ok(match value.as_integer() {
        Some(version) if version >= 0 => version,
        _ => 0,
    })
}

impl Default for PermissionState {
    fn default() -> Self {
        Self {
            edit_policy: EditPolicy::default(),
            allow_bash_execute: false,
            allowed_bash_commands: HashSet::new(),
            disallowed_bash_commands: HashSet::new(),
            allowed_bash_globs: HashSet::new(),
            allowed_web_fetch_domains: HashSet::new(),
            allowed_mcp_tools: HashSet::new(),
            allowed_mcp_servers: HashSet::new(),
            validated_mcp_server_grants_version: VALIDATED_MCP_SERVER_GRANTS_VERSION,
        }
    }
}

fn state_dir_for_cwd(cwd: &AbsPathBuf) -> std::path::PathBuf {
    xai_grok_config::sessions_cwd_dir(cwd.as_str())
}

fn sanitize_client_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn state_file_path(dir: &std::path::Path, client_identifier: Option<&str>) -> std::path::PathBuf {
    match client_identifier {
        Some(id) => dir.join(format!("permission_{}.toml", sanitize_client_id(id))),
        None => dir.join("permission.toml"),
    }
}

async fn try_load_state_with_writer<F>(path: &std::path::Path, writer: F) -> Option<PermissionState>
where
    F: FnOnce(&std::path::Path, &str) -> std::io::Result<()> + Send + 'static,
{
    match tokio::fs::read_to_string(path).await {
        Ok(s) => {
            let mut state: PermissionState = toml::from_str(&s).unwrap_or_default();
            if state.validated_mcp_server_grants_version < VALIDATED_MCP_SERVER_GRANTS_VERSION {
                state.allowed_mcp_servers.clear();
                state.validated_mcp_server_grants_version = VALIDATED_MCP_SERVER_GRANTS_VERSION;
                tracing::info!(path = %path.display(), "invalidated legacy MCP server grants");
                if let Err(e) = persist_state_to_path_with_writer(path, &state, writer).await {
                    tracing::warn!(?e, path = %path.display(), "failed writing permission state");
                }
            }
            Some(state)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            tracing::warn!(?e, "failed reading permission state");
            None
        }
    }
}

async fn try_load_state(path: &std::path::Path) -> Option<PermissionState> {
    try_load_state_with_writer(path, |path, contents| {
        xai_grok_config::fs_atomic::write_atomically(path, contents, None)
    })
    .await
}

async fn load_state_from_dir(
    dir: &std::path::Path,
    client_identifier: Option<&str>,
) -> PermissionState {
    if let Some(id) = client_identifier {
        let per_client = state_file_path(dir, Some(id));
        if let Some(state) = try_load_state(&per_client).await {
            return state;
        }
    }
    try_load_state(&state_file_path(dir, None))
        .await
        .unwrap_or_default()
}

pub(crate) async fn load_state_from_disk(
    cwd: &AbsPathBuf,
    client_identifier: Option<&str>,
) -> PermissionState {
    load_state_from_dir(&state_dir_for_cwd(cwd), client_identifier).await
}

async fn persist_state_to_path_with_writer<F>(
    path: &std::path::Path,
    state: &PermissionState,
    writer: F,
) -> std::io::Result<()>
where
    F: FnOnce(&std::path::Path, &str) -> std::io::Result<()> + Send + 'static,
{
    let contents = toml::to_string_pretty(state)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || writer(&path, &contents))
        .await
        .map_err(std::io::Error::other)?
}

async fn persist_state_to_path(
    path: &std::path::Path,
    state: &PermissionState,
) -> std::io::Result<()> {
    persist_state_to_path_with_writer(path, state, |path, contents| {
        xai_grok_config::fs_atomic::write_atomically(path, contents, None)
    })
    .await
}

async fn persist_state_to_dir(
    dir: &std::path::Path,
    state: &PermissionState,
    client_identifier: Option<&str>,
) {
    if let Err(e) = tokio::fs::create_dir_all(dir).await {
        tracing::warn!(?e, "failed creating permission state directory");
        return;
    }
    let path = state_file_path(dir, client_identifier);
    if let Err(e) = persist_state_to_path(&path, state).await {
        tracing::warn!(?e, path = %path.display(), "failed writing permission state");
    }
}

pub(crate) async fn persist_state(
    cwd: &AbsPathBuf,
    state: &PermissionState,
    client_identifier: Option<&str>,
) {
    persist_state_to_dir(&state_dir_for_cwd(cwd), state, client_identifier).await
}

pub async fn cleanup_stale_permission_state(max_age: std::time::Duration) {
    let sessions_dir = grok_home().join("sessions");
    let Ok(mut entries) = tokio::fs::read_dir(&sessions_dir).await else {
        return;
    };
    while let Ok(Some(session_entry)) = entries.next_entry().await {
        let Ok(ft) = session_entry.file_type().await else {
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        let session_dir = session_entry.path();
        let Ok(mut files) = tokio::fs::read_dir(&session_dir).await else {
            continue;
        };
        while let Ok(Some(file_entry)) = files.next_entry().await {
            let path = file_entry.path();
            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !file_name.starts_with("permission") || !file_name.ends_with(".toml") {
                continue;
            }
            if let Ok(metadata) = tokio::fs::metadata(&path).await
                && let Ok(modified) = metadata.modified()
                && let Ok(age) = modified.elapsed()
                && age > max_age
            {
                tracing::debug!(path = %path.display(), "removing stale permission state");
                let _ = tokio::fs::remove_file(&path).await;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    // ── PermissionState serialization roundtrip tests ─────────────

    #[test]
    fn default_state_serialization() {
        let state = PermissionState::default();
        let toml_str = toml::to_string_pretty(&state).unwrap();
        let restored: PermissionState = toml::from_str(&toml_str).unwrap();
        assert!(!restored.allow_bash_execute);
        assert!(restored.allowed_bash_commands.is_empty());
        assert!(restored.disallowed_bash_commands.is_empty());
        assert_eq!(
            restored.validated_mcp_server_grants_version,
            VALIDATED_MCP_SERVER_GRANTS_VERSION
        );
    }

    #[test]
    fn roundtrip_with_allowed_commands() {
        let mut state = PermissionState::default();
        state.allow_bash_execute = true;
        state.allowed_bash_commands.insert("cargo test".to_string());
        state
            .allowed_bash_commands
            .insert("npm run build".to_string());

        let toml_str = toml::to_string_pretty(&state).unwrap();
        let restored: PermissionState = toml::from_str(&toml_str).unwrap();

        assert!(restored.allow_bash_execute);
        assert!(restored.allowed_bash_commands.contains("cargo test"));
        assert!(restored.allowed_bash_commands.contains("npm run build"));
        assert_eq!(restored.allowed_bash_commands.len(), 2);
    }

    #[test]
    fn roundtrip_with_disallowed_commands() {
        let mut state = PermissionState::default();
        state.disallowed_bash_commands.insert("rm -rf".to_string());
        state
            .disallowed_bash_commands
            .insert("git push --force".to_string());

        let toml_str = toml::to_string_pretty(&state).unwrap();
        let restored: PermissionState = toml::from_str(&toml_str).unwrap();

        let denied = &restored.disallowed_bash_commands;
        assert!(denied.contains("rm -rf"));
        assert!(denied.contains("git push --force"));
        assert_eq!(denied.len(), 2);
    }

    #[test]
    fn roundtrip_with_both_allowed_and_disallowed() {
        // Simulate a real scenario: some commands explicitly allowed,
        // others explicitly denied.
        let mut state = PermissionState::default();
        state.allow_bash_execute = false;
        state.allowed_bash_commands.insert("cargo test".to_string());
        state.allowed_bash_commands.insert("git status".to_string());
        state
            .disallowed_bash_commands
            .insert("rm -rf /".to_string());
        state.disallowed_bash_commands.insert("curl".to_string());

        let toml_str = toml::to_string_pretty(&state).unwrap();
        let restored: PermissionState = toml::from_str(&toml_str).unwrap();

        assert!(!restored.allow_bash_execute);
        assert_eq!(restored.allowed_bash_commands.len(), 2);
        assert_eq!(restored.disallowed_bash_commands.len(), 2);
        assert!(restored.allowed_bash_commands.contains("cargo test"));
        assert!(restored.disallowed_bash_commands.contains("curl"));
    }

    #[test]
    fn edit_policy_is_persisted() {
        let mut state = PermissionState::default();
        state.edit_policy = EditPolicy::Allow;

        let toml_str = toml::to_string_pretty(&state).unwrap();
        assert!(toml_str.contains("edit_policy"));

        let restored: PermissionState = toml::from_str(&toml_str).unwrap();
        assert_eq!(restored.edit_policy, EditPolicy::Allow);
    }

    #[test]
    fn edit_policy_reject_roundtrip() {
        let mut state = PermissionState::default();
        state.edit_policy = EditPolicy::Reject;

        let toml_str = toml::to_string_pretty(&state).unwrap();
        let restored: PermissionState = toml::from_str(&toml_str).unwrap();
        assert_eq!(restored.edit_policy, EditPolicy::Reject);
    }

    #[test]
    fn missing_edit_policy_defaults_to_ask() {
        let toml_str = r#"allow_bash_execute = false"#;
        let state: PermissionState = toml::from_str(toml_str).unwrap();
        assert_eq!(state.edit_policy, EditPolicy::Ask);
    }

    #[test]
    fn deserialize_from_empty_toml_is_legacy() {
        let state: PermissionState = toml::from_str("").unwrap();
        assert!(!state.allow_bash_execute);
        assert!(state.allowed_bash_commands.is_empty());
        assert!(state.disallowed_bash_commands.is_empty());
        assert_eq!(state.validated_mcp_server_grants_version, 0);
    }

    #[test]
    fn deserialize_partial_toml() {
        // Only some fields present — others should default.
        let toml_str = r#"allow_bash_execute = true"#;
        let state: PermissionState = toml::from_str(toml_str).unwrap();
        assert!(state.allow_bash_execute);
        assert!(state.allowed_bash_commands.is_empty());
        assert!(state.disallowed_bash_commands.is_empty());
    }

    #[test]
    fn roundtrip_with_allowed_web_fetch_domains() {
        let mut state = PermissionState::default();
        state
            .allowed_web_fetch_domains
            .insert("stackoverflow.com".to_string());
        state
            .allowed_web_fetch_domains
            .insert("custom.example.com".to_string());

        let toml_str = toml::to_string_pretty(&state).unwrap();
        let restored: PermissionState = toml::from_str(&toml_str).unwrap();

        assert_eq!(restored.allowed_web_fetch_domains.len(), 2);
        assert!(
            restored
                .allowed_web_fetch_domains
                .contains("stackoverflow.com")
        );
        assert!(
            restored
                .allowed_web_fetch_domains
                .contains("custom.example.com")
        );
    }

    #[test]
    fn roundtrip_with_allowed_mcp_tools() {
        let mut state = PermissionState::default();
        state
            .allowed_mcp_tools
            .insert("grok_com_notion__notion-fetch".to_string());
        state
            .allowed_mcp_tools
            .insert("linear__list_issues".to_string());

        let toml_str = toml::to_string_pretty(&state).unwrap();
        let restored: PermissionState = toml::from_str(&toml_str).unwrap();

        assert_eq!(restored.allowed_mcp_tools.len(), 2);
        assert!(
            restored
                .allowed_mcp_tools
                .contains("grok_com_notion__notion-fetch")
        );
        assert!(restored.allowed_mcp_tools.contains("linear__list_issues"));
        assert!(restored.allowed_mcp_servers.is_empty());
    }

    #[test]
    fn roundtrip_with_allowed_mcp_servers() {
        let mut state = PermissionState::default();
        state
            .allowed_mcp_servers
            .insert("grok_com_slack".to_string());
        state.allowed_mcp_servers.insert("linear".to_string());

        let toml_str = toml::to_string_pretty(&state).unwrap();
        let restored: PermissionState = toml::from_str(&toml_str).unwrap();

        assert_eq!(restored.allowed_mcp_servers.len(), 2);
        assert!(restored.allowed_mcp_servers.contains("grok_com_slack"));
        assert!(restored.allowed_mcp_servers.contains("linear"));
        assert!(restored.allowed_mcp_tools.is_empty());
    }

    #[test]
    fn roundtrip_with_both_mcp_sets() {
        let mut state = PermissionState::default();
        state.allowed_mcp_tools.insert("notion__fetch".to_string());
        state.allowed_mcp_servers.insert("linear".to_string());

        let toml_str = toml::to_string_pretty(&state).unwrap();
        let restored: PermissionState = toml::from_str(&toml_str).unwrap();

        assert_eq!(restored.allowed_mcp_tools.len(), 1);
        assert_eq!(restored.allowed_mcp_servers.len(), 1);
        assert!(restored.allowed_mcp_tools.contains("notion__fetch"));
        assert!(restored.allowed_mcp_servers.contains("linear"));
    }

    #[test]
    fn deserialize_old_state_without_mcp_fields() {
        // A state file from a binary that predates this design has
        // neither MCP field. #[serde(default)] should yield empty sets.
        let toml_str = r#"
allow_bash_execute = true
allowed_bash_commands = ["cargo test"]
allowed_web_fetch_domains = ["github.com"]
"#;
        let state: PermissionState = toml::from_str(toml_str).unwrap();
        assert!(state.allow_bash_execute);
        assert!(state.allowed_bash_commands.contains("cargo test"));
        assert!(state.allowed_web_fetch_domains.contains("github.com"));
        assert!(state.allowed_mcp_tools.is_empty());
        assert!(state.allowed_mcp_servers.is_empty());
        assert_eq!(state.validated_mcp_server_grants_version, 0);
    }

    #[test]
    fn malformed_mcp_server_grants_version_is_legacy() {
        for marker in ["-1", "\"invalid\""] {
            let state: PermissionState =
                toml::from_str(&format!("validated_mcp_server_grants_version = {marker}")).unwrap();
            assert_eq!(state.validated_mcp_server_grants_version, 0);
        }
    }

    #[test]
    fn deserialize_unknown_fields_tolerated() {
        // PermissionState uses #[serde(default)] which provides defaults for
        // missing fields. It does NOT use #[serde(deny_unknown_fields)], so
        // unknown keys in TOML are silently ignored. This is important for
        // forward compatibility: older versions of the binary should be able
        // to read state files written by newer versions that may have added
        // new fields.
        let toml_str = r#"
allow_bash_execute = false
unknown_field = "should be ignored"
allowed_bash_commands = ["ls"]
"#;
        let state: PermissionState = toml::from_str(toml_str).unwrap();
        assert!(!state.allow_bash_execute);
        assert!(state.allowed_bash_commands.contains("ls"));
        assert!(state.disallowed_bash_commands.is_empty());
    }

    // ── Disk persistence roundtrip tests ─────────────────────────

    async fn write_legacy_mcp_state(path: &std::path::Path) {
        tokio::fs::write(
            path,
            r#"
edit_policy = "reject"
allow_bash_execute = true
allowed_bash_commands = ["cargo test"]
disallowed_bash_commands = ["rm"]
allowed_web_fetch_domains = ["example.com"]
allowed_mcp_tools = ["a__b__c"]
allowed_mcp_servers = ["a"]
"#,
        )
        .await
        .unwrap();
    }

    fn assert_legacy_mcp_state_migrated(state: &PermissionState) {
        assert!(state.allowed_mcp_servers.is_empty());
        assert!(state.allowed_mcp_tools.contains("a__b__c"));
        assert!(state.allow_bash_execute);
        assert!(state.allowed_bash_commands.contains("cargo test"));
        assert!(state.disallowed_bash_commands.contains("rm"));
        assert!(state.allowed_web_fetch_domains.contains("example.com"));
        assert_eq!(state.edit_policy, EditPolicy::Reject);
        assert_eq!(
            state.validated_mcp_server_grants_version,
            VALIDATED_MCP_SERVER_GRANTS_VERSION
        );
    }

    #[tokio::test]
    async fn legacy_shared_mcp_server_grants_migrate_and_rewrite() {
        let tmp = tempfile::tempdir().unwrap();
        let path = state_file_path(tmp.path(), None);
        write_legacy_mcp_state(&path).await;

        assert_legacy_mcp_state_migrated(&load_state_from_dir(tmp.path(), None).await);
        let rewritten: PermissionState =
            toml::from_str(&tokio::fs::read_to_string(&path).await.unwrap()).unwrap();
        assert_legacy_mcp_state_migrated(&rewritten);
    }

    #[tokio::test]
    async fn failed_migration_rewrite_preserves_legacy_file_for_retry() {
        fn fail_write(_: &std::path::Path, _: &str) -> std::io::Result<()> {
            Err(std::io::Error::other("injected write failure"))
        }

        let tmp = tempfile::tempdir().unwrap();
        let path = state_file_path(tmp.path(), None);
        write_legacy_mcp_state(&path).await;
        let legacy_contents = tokio::fs::read_to_string(&path).await.unwrap();

        let in_memory = try_load_state_with_writer(&path, fail_write).await.unwrap();
        assert_legacy_mcp_state_migrated(&in_memory);
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            legacy_contents
        );
        let still_legacy: PermissionState = toml::from_str(&legacy_contents).unwrap();
        assert_eq!(still_legacy.validated_mcp_server_grants_version, 0);
        assert!(still_legacy.allowed_mcp_servers.contains("a"));

        assert_legacy_mcp_state_migrated(&try_load_state(&path).await.unwrap());
        let rewritten: PermissionState =
            toml::from_str(&tokio::fs::read_to_string(&path).await.unwrap()).unwrap();
        assert_legacy_mcp_state_migrated(&rewritten);
    }

    #[tokio::test]
    async fn current_and_future_mcp_server_grants_are_retained_exactly() {
        for version in [
            VALIDATED_MCP_SERVER_GRANTS_VERSION,
            VALIDATED_MCP_SERVER_GRANTS_VERSION + 1,
            4_294_967_296,
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let mut state = PermissionState::default();
            state.validated_mcp_server_grants_version = version;
            state.allowed_mcp_servers.insert("linear".to_owned());
            persist_state_to_dir(tmp.path(), &state, None).await;

            let loaded = load_state_from_dir(tmp.path(), None).await;
            assert!(loaded.allowed_mcp_servers.contains("linear"));
            assert_eq!(loaded.validated_mcp_server_grants_version, version);
            let persisted: PermissionState = toml::from_str(
                &tokio::fs::read_to_string(state_file_path(tmp.path(), None))
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(persisted.validated_mcp_server_grants_version, version);
        }
    }

    #[tokio::test]
    async fn per_client_legacy_migration_rewrites_only_loaded_file() {
        let tmp = tempfile::tempdir().unwrap();
        let shared = state_file_path(tmp.path(), None);
        let per_client = state_file_path(tmp.path(), Some("desktop"));
        let mut shared_state = PermissionState::default();
        shared_state.allowed_mcp_servers.insert("shared".to_owned());
        persist_state_to_dir(tmp.path(), &shared_state, None).await;
        write_legacy_mcp_state(&per_client).await;

        assert_legacy_mcp_state_migrated(&load_state_from_dir(tmp.path(), Some("desktop")).await);
        let shared_after: PermissionState =
            toml::from_str(&tokio::fs::read_to_string(shared).await.unwrap()).unwrap();
        assert!(shared_after.allowed_mcp_servers.contains("shared"));
        let client_after: PermissionState =
            toml::from_str(&tokio::fs::read_to_string(per_client).await.unwrap()).unwrap();
        assert_legacy_mcp_state_migrated(&client_after);
    }

    #[tokio::test]
    async fn per_client_fallback_migrates_shared_file() {
        let tmp = tempfile::tempdir().unwrap();
        let shared = state_file_path(tmp.path(), None);
        write_legacy_mcp_state(&shared).await;

        assert_legacy_mcp_state_migrated(
            &load_state_from_dir(tmp.path(), Some("missing-client")).await,
        );
        let shared_after: PermissionState =
            toml::from_str(&tokio::fs::read_to_string(shared).await.unwrap()).unwrap();
        assert_legacy_mcp_state_migrated(&shared_after);
        assert!(!state_file_path(tmp.path(), Some("missing-client")).exists());
    }

    #[tokio::test]
    async fn persist_and_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = PermissionState::default();
        state.allow_bash_execute = true;
        state
            .allowed_bash_commands
            .insert("cargo build".to_string());
        state.disallowed_bash_commands.insert("rm -rf".to_string());

        persist_state_to_dir(tmp.path(), &state, None).await;
        let restored = load_state_from_dir(tmp.path(), None).await;
        assert!(restored.allow_bash_execute);
        assert!(restored.allowed_bash_commands.contains("cargo build"));
        assert!(restored.disallowed_bash_commands.contains("rm -rf"));
    }

    #[tokio::test]
    async fn load_missing_file_returns_default() {
        // Simulates load_state_from_disk behavior for a missing file.
        let path = std::path::Path::new("/nonexistent/permission.toml");
        let result = tokio::fs::read_to_string(path).await;
        match result {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let state = PermissionState::default();
                assert!(!state.allow_bash_execute);
            }
            _ => panic!("expected NotFound error"),
        }
    }

    #[tokio::test]
    async fn load_corrupt_file_returns_default() {
        // Simulates load_state_from_disk behavior for corrupt TOML.
        let corrupt = "this is not valid toml {{{{";
        let state: PermissionState = toml::from_str(corrupt).unwrap_or_default();
        assert!(!state.allow_bash_execute);
        assert!(state.allowed_bash_commands.is_empty());
    }

    // ── Per-client state file path tests ──────────────────────────

    #[test]
    fn state_file_path_without_client_id() {
        let tmp = tempfile::tempdir().unwrap();
        let path = state_file_path(tmp.path(), None);
        assert_eq!(path.file_name().unwrap(), "permission.toml");
    }

    #[test]
    fn state_file_path_with_client_id() {
        let tmp = tempfile::tempdir().unwrap();
        let path = state_file_path(tmp.path(), Some("vscode-ext"));
        assert_eq!(path.file_name().unwrap(), "permission_vscode-ext.toml");
    }

    #[test]
    fn state_file_path_empty_client_id() {
        let tmp = tempfile::tempdir().unwrap();
        let path = state_file_path(tmp.path(), Some(""));
        assert_eq!(path.file_name().unwrap(), "permission_.toml");
    }

    #[test]
    fn state_file_path_sanitizes_path_separators() {
        let tmp = tempfile::tempdir().unwrap();
        let path = state_file_path(tmp.path(), Some("foo/bar"));
        assert_eq!(path.file_name().unwrap(), "permission_foo_bar.toml");

        let path = state_file_path(tmp.path(), Some("foo\\bar"));
        assert_eq!(path.file_name().unwrap(), "permission_foo_bar.toml");
    }

    #[test]
    fn sanitize_client_id_prevents_traversal() {
        assert_eq!(sanitize_client_id("foo/../../attack"), "foo_______attack");
        assert_eq!(sanitize_client_id("normal-id"), "normal-id");
        assert_eq!(sanitize_client_id("has\0null"), "has_null");
        assert_eq!(sanitize_client_id("back\\slash"), "back_slash");
    }

    #[tokio::test]
    async fn try_load_state_missing_returns_none() {
        let result = try_load_state(std::path::Path::new("/nonexistent/permission.toml")).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn try_load_state_valid_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("permission.toml");
        let mut expected = PermissionState::default();
        expected.allow_bash_execute = true;
        tokio::fs::write(&path, toml::to_string_pretty(&expected).unwrap())
            .await
            .unwrap();
        let state = try_load_state(&path).await.unwrap();
        assert!(state.allow_bash_execute);
        assert_eq!(
            state.validated_mcp_server_grants_version,
            VALIDATED_MCP_SERVER_GRANTS_VERSION
        );
    }

    #[tokio::test]
    async fn per_client_persist_and_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        let mut state = PermissionState::default();
        state.allow_bash_execute = true;
        state.allowed_bash_commands.insert("cargo test".to_string());

        persist_state_to_dir(dir, &state, Some("client_a")).await;

        let loaded = load_state_from_dir(dir, Some("client_a")).await;
        assert!(loaded.allow_bash_execute);
        assert!(loaded.allowed_bash_commands.contains("cargo test"));
    }

    #[tokio::test]
    async fn per_client_load_falls_back_to_shared() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        let mut shared_state = PermissionState::default();
        shared_state.allow_bash_execute = true;
        shared_state
            .allowed_bash_commands
            .insert("cargo test".to_string());
        persist_state_to_dir(dir, &shared_state, None).await;

        let loaded = load_state_from_dir(dir, Some("new_client")).await;
        assert!(loaded.allow_bash_execute);
        assert!(loaded.allowed_bash_commands.contains("cargo test"));
    }

    #[tokio::test]
    async fn per_client_file_takes_priority_over_shared() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        let mut shared_state = PermissionState::default();
        shared_state.allow_bash_execute = true;
        persist_state_to_dir(dir, &shared_state, None).await;

        let mut client_state = PermissionState::default();
        client_state.allow_bash_execute = false;
        client_state
            .allowed_bash_commands
            .insert("npm test".to_string());
        persist_state_to_dir(dir, &client_state, Some("my-client")).await;

        let loaded = load_state_from_dir(dir, Some("my-client")).await;
        assert!(!loaded.allow_bash_execute);
        assert!(loaded.allowed_bash_commands.contains("npm test"));

        let shared_loaded = load_state_from_dir(dir, None).await;
        assert!(shared_loaded.allow_bash_execute);
    }

    #[tokio::test]
    async fn load_none_client_returns_default_when_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let loaded = load_state_from_dir(tmp.path(), None).await;
        assert!(!loaded.allow_bash_execute);
        assert!(loaded.allowed_bash_commands.is_empty());
    }

    #[tokio::test]
    async fn per_client_isolation_between_clients() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        let mut state_a = PermissionState::default();
        state_a
            .allowed_bash_commands
            .insert("cargo test".to_string());
        persist_state_to_dir(dir, &state_a, Some("client_a")).await;

        let mut state_b = PermissionState::default();
        state_b.allowed_bash_commands.insert("npm test".to_string());
        persist_state_to_dir(dir, &state_b, Some("client_b")).await;

        let loaded_a = load_state_from_dir(dir, Some("client_a")).await;
        assert!(loaded_a.allowed_bash_commands.contains("cargo test"));
        assert!(!loaded_a.allowed_bash_commands.contains("npm test"));

        let loaded_b = load_state_from_dir(dir, Some("client_b")).await;
        assert!(loaded_b.allowed_bash_commands.contains("npm test"));
        assert!(!loaded_b.allowed_bash_commands.contains("cargo test"));
    }
}
