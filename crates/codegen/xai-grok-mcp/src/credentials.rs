//! Persistent credential storage for MCP server OAuth tokens.
//!
//! Credentials are stored in `$GROK_HOME/mcp_credentials.json`, keyed by a
//! composite key derived from the server name and URL. This keeps MCP OAuth
//! tokens isolated from the user's xAI auth (`auth.json`).
//!
//! Stores rmcp's `StoredCredentials` type directly — the same type that
//! rmcp's `AuthorizationManager` uses internally.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::rmcp;

/// Ensure credential paths are owner-only (Unix `0o600`).
///
/// Local helper (not shell-base): `xai-grok-mcp` sits below `config-types` in the
/// dep graph, and shell-base pulls shared→config-types→mcp — a cycle if linked.
/// Windows ACL tightening stays on auth via shell-base; MCP is Unix-first here.
fn ensure_owner_only_permissions(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(path) {
            Ok(metadata) => {
                let mode = metadata.permissions().mode();
                if mode & 0o777 != 0o600 {
                    let mut perms = metadata.permissions();
                    perms.set_mode(0o600);
                    std::fs::set_permissions(path, perms)?;
                }
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

type Result<T> = std::result::Result<T, McpCredentialError>;

#[derive(Debug, thiserror::Error)]
pub enum McpCredentialError {
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// File name for the credential store inside `$GROK_HOME`.
const CREDENTIALS_FILENAME: &str = "mcp_credentials.json";

/// On-disk credential store: `$GROK_HOME/mcp_credentials.json`.
///
/// Stores rmcp `StoredCredentials` per MCP server, keyed by
/// `"{server_name}:{server_url}"`.
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct McpCredentialStore {
    #[serde(flatten)]
    entries: BTreeMap<String, rmcp::transport::auth::StoredCredentials>,
}

impl std::fmt::Debug for McpCredentialStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpCredentialStore")
            .field("entry_count", &self.entries.len())
            .finish()
    }
}

impl McpCredentialStore {
    /// Build the composite key for a credential entry.
    pub fn key(server_name: &str, server_url: &Url) -> String {
        format!("{}:{}", server_name, server_url)
    }

    /// Load the credential store from the default path (`$GROK_HOME/mcp_credentials.json`).
    ///
    /// Returns an empty store if the file does not exist.
    pub fn load_default() -> Result<Self> {
        match Self::default_path() {
            Some(path) => Self::load_from(&path),
            None => Ok(Self::default()),
        }
    }

    /// Load from a specific path.
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)?;
        // Tighten world-readable credential files on load (hand copies, etc.).
        // Best-effort: chmod failure must not block using existing tokens.
        if let Err(e) = ensure_owner_only_permissions(path) {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "mcp credentials: failed to enforce owner-only permissions"
            );
        }
        let store: McpCredentialStore = serde_json::from_str(&content)?;
        Ok(store)
    }

    /// Save the credential store to the default path.
    pub fn save_default(&self) -> Result<()> {
        let path = Self::default_path().ok_or_else(|| {
            McpCredentialError::Other("no user grok home (set $GROK_HOME or $HOME)".into())
        })?;
        self.save_to(&path)
    }

    /// Atomically insert a credential and save — safe for concurrent use.
    ///
    /// Instead of the caller doing `insert_rmcp` + `save_default` (which races
    /// with other processes), this method:
    /// 1. Acquires a file lock on `mcp_credentials.json.lock`
    /// 2. Reloads the store from disk (picks up other processes' writes)
    /// 3. Inserts the new entry
    /// 4. Saves atomically (temp + rename)
    /// 5. Updates `self` with the merged result
    /// 6. Releases the lock
    pub fn insert_and_save(
        &mut self,
        server_name: &str,
        server_url: &url::Url,
        creds: rmcp::transport::auth::StoredCredentials,
    ) -> Result<()> {
        let path = Self::default_path().ok_or_else(|| {
            McpCredentialError::Other("no user grok home (set $GROK_HOME or $HOME)".into())
        })?;
        let lock_path = path.with_extension("lock");

        // Ensure parent dir exists.
        if let Some(parent) = lock_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;

            let lock_file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&lock_path)?;
            let fd = lock_file.as_raw_fd();
            loop {
                if unsafe { libc::flock(fd, libc::LOCK_EX) } == 0 {
                    break;
                }
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue; // Retry on EINTR.
                }
                // Lock failed for another reason — fall back to non-atomic insert.
                self.insert_rmcp(server_name, server_url, creds);
                return self.save_to(&path);
            }

            // Reload from disk under lock to merge with concurrent writes.
            let mut fresh = Self::load_from(&path).unwrap_or_default();
            fresh.insert_rmcp(server_name, server_url, creds);
            fresh.save_to(&path)?;
            *self = fresh;

            // Lock released when lock_file is dropped.
        }

        #[cfg(not(unix))]
        {
            // No flock on non-unix — best-effort.
            self.insert_rmcp(server_name, server_url, creds);
            self.save_to(&path)?;
        }

        Ok(())
    }

    /// Save to a specific path.
    ///
    /// Writes atomically via temp file + rename to prevent credential loss on
    /// crash. On Unix, the temp file is created with 0600 permissions from the
    /// start (no TOCTOU window where secrets are world-readable).
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let write_path = xai_grok_config::fs_atomic::resolve_write_target(path)?;
        if let Some(parent) = write_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = serde_json::to_string_pretty(self)?;
        let tmp_path = write_path.with_extension("tmp");

        {
            use std::io::Write;

            #[cfg(unix)]
            let file = {
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&tmp_path)?
            };
            #[cfg(not(unix))]
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp_path)?;

            let mut writer = std::io::BufWriter::new(file);
            writer.write_all(content.as_bytes())?;
            writer.flush()?;
        }

        // `mode(0o600)` only applies on create; tighten before rename.
        // Fail hard on tmp: credentials are not published yet.
        ensure_owner_only_permissions(&tmp_path)?;
        std::fs::rename(&tmp_path, &write_path)?;
        // Best-effort after rename: new tokens are already published.
        if let Err(e) = ensure_owner_only_permissions(&write_path) {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "mcp: failed to ensure owner-only permissions after credential save"
            );
        }
        Ok(())
    }

    /// Look up credentials for a server.
    pub fn get(
        &self,
        server_name: &str,
        server_url: &Url,
    ) -> Option<&rmcp::transport::auth::StoredCredentials> {
        self.entries.get(&Self::key(server_name, server_url))
    }

    /// Insert rmcp `StoredCredentials` for a server.
    pub fn insert_rmcp(
        &mut self,
        server_name: &str,
        server_url: &Url,
        creds: rmcp::transport::auth::StoredCredentials,
    ) {
        self.entries
            .insert(Self::key(server_name, server_url), creds);
    }

    /// Check if credentials exist for a server (regardless of expiry).
    pub fn has_credentials(&self, server_name: &str, server_url: &Url) -> bool {
        self.entries
            .contains_key(&Self::key(server_name, server_url))
    }

    /// Remove credentials for a server.
    pub fn remove(&mut self, server_name: &str, server_url: &Url) {
        self.entries.remove(&Self::key(server_name, server_url));
    }

    /// Remove all credentials for a server by name (any URL).
    pub fn remove_by_server_name(&mut self, server_name: &str) -> usize {
        let prefix = format!("{server_name}:");
        let before = self.entries.len();
        self.entries.retain(|k, _| !k.starts_with(&prefix));
        before - self.entries.len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Default path: `$GROK_HOME/mcp_credentials.json`.
    fn default_path() -> Option<PathBuf> {
        Some(xai_grok_config::user_grok_home()?.join(CREDENTIALS_FILENAME))
    }
}

/// Adapter implementing rmcp's `CredentialStore` trait backed by the on-disk
/// `McpCredentialStore`. Each adapter instance is scoped to a single MCP server
/// (keyed by name + URL); rmcp's `AuthorizationManager` calls load/save/clear
/// transparently during token exchange and refresh.
pub struct McpCredentialStoreAdapter {
    server_name: String,
    server_url: url::Url,
}

impl McpCredentialStoreAdapter {
    pub fn new(server_name: String, server_url: url::Url) -> Self {
        Self {
            server_name,
            server_url,
        }
    }
}

#[async_trait::async_trait]
impl rmcp::transport::auth::CredentialStore for McpCredentialStoreAdapter {
    async fn load(
        &self,
    ) -> std::result::Result<
        Option<rmcp::transport::auth::StoredCredentials>,
        rmcp::transport::auth::AuthError,
    > {
        let name = self.server_name.clone();
        let url = self.server_url.clone();
        tokio::task::spawn_blocking(move || {
            let store = McpCredentialStore::load_default()
                .map_err(|e| rmcp::transport::auth::AuthError::InternalError(e.to_string()))?;
            Ok(store.get(&name, &url).cloned())
        })
        .await
        .map_err(|e| rmcp::transport::auth::AuthError::InternalError(e.to_string()))?
    }

    async fn save(
        &self,
        credentials: rmcp::transport::auth::StoredCredentials,
    ) -> std::result::Result<(), rmcp::transport::auth::AuthError> {
        let name = self.server_name.clone();
        let url = self.server_url.clone();
        tokio::task::spawn_blocking(move || {
            let mut store = McpCredentialStore::load_default().unwrap_or_default();
            store
                .insert_and_save(&name, &url, credentials)
                .map_err(|e| rmcp::transport::auth::AuthError::InternalError(e.to_string()))
        })
        .await
        .map_err(|e| rmcp::transport::auth::AuthError::InternalError(e.to_string()))?
    }

    async fn clear(&self) -> std::result::Result<(), rmcp::transport::auth::AuthError> {
        let name = self.server_name.clone();
        let url = self.server_url.clone();
        tokio::task::spawn_blocking(move || {
            let mut store = McpCredentialStore::load_default().unwrap_or_default();
            store.remove(&name, &url);
            store
                .save_default()
                .map_err(|e| rmcp::transport::auth::AuthError::InternalError(e.to_string()))
        })
        .await
        .map_err(|e| rmcp::transport::auth::AuthError::InternalError(e.to_string()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_stored_creds(client_id: &str) -> rmcp::transport::auth::StoredCredentials {
        rmcp::transport::auth::StoredCredentials::new(client_id.to_string(), None, Vec::new(), None)
    }

    #[test]
    fn insert_and_get() {
        let mut store = McpCredentialStore::default();
        let url = Url::parse("https://test.example.com/mcp").unwrap();
        store.insert_rmcp("test", &url, test_stored_creds("test-client"));
        assert!(store.get("test", &url).is_some());
        assert_eq!(store.get("test", &url).unwrap().client_id, "test-client");
    }

    #[test]
    fn remove_entry() {
        let mut store = McpCredentialStore::default();
        let url = Url::parse("https://test.example.com/mcp").unwrap();
        store.insert_rmcp("test", &url, test_stored_creds("test-client"));
        store.remove("test", &url);
        assert!(store.get("test", &url).is_none());
    }

    #[test]
    fn has_credentials() {
        let mut store = McpCredentialStore::default();
        let url = Url::parse("https://test.example.com/mcp").unwrap();
        assert!(!store.has_credentials("test", &url));
        store.insert_rmcp("test", &url, test_stored_creds("c"));
        assert!(store.has_credentials("test", &url));
    }

    #[test]
    fn roundtrip_serialization() {
        let mut store = McpCredentialStore::default();
        let url = Url::parse("https://test.example.com/mcp").unwrap();
        store.insert_rmcp("test", &url, test_stored_creds("test-client"));

        let json = serde_json::to_string(&store).unwrap();
        let loaded: McpCredentialStore = serde_json::from_str(&json).unwrap();
        assert!(loaded.get("test", &url).is_some());
    }

    /// Raw JSON fixture in the exact shape rmcp 0.17 persisted to
    /// `$GROK_HOME/mcp_credentials.json`. Existing credential files must keep
    /// loading across rmcp upgrades (2.1's `OAuthTokenResponse` gained vendor
    /// extra token fields), so this must be a string literal — never JSON
    /// serialized by the current code.
    #[test]
    fn legacy_on_disk_fixture_still_deserializes() {
        use oauth2::TokenResponse as _;

        let fixture = r#"{
            "linear:https://mcp.example.com/mcp": {
                "client_id": "legacy-client-id",
                "token_response": {
                    "access_token": "at-123",
                    "token_type": "bearer",
                    "expires_in": 3600,
                    "refresh_token": "rt-456",
                    "scope": "read write"
                },
                "granted_scopes": ["read", "write"],
                "token_received_at": 1730000000
            },
            "noauth:https://example.com/mcp": {
                "client_id": "c2",
                "token_response": null
            }
        }"#;

        let store: McpCredentialStore = serde_json::from_str(fixture).unwrap();
        let url = Url::parse("https://mcp.example.com/mcp").unwrap();
        let creds = store.get("linear", &url).expect("legacy entry loads");
        assert_eq!(creds.client_id, "legacy-client-id");
        let token = creds.token_response.as_ref().expect("token loads");
        assert_eq!(token.access_token().secret(), "at-123");
        assert_eq!(token.refresh_token().unwrap().secret(), "rt-456");
        assert_eq!(creds.granted_scopes, vec!["read", "write"]);
        assert_eq!(creds.token_received_at, Some(1730000000));

        // Entry without the `#[serde(default)]` fields on disk still loads.
        let url2 = Url::parse("https://example.com/mcp").unwrap();
        let creds2 = store.get("noauth", &url2).expect("minimal entry loads");
        assert!(creds2.token_response.is_none());
        assert!(creds2.granted_scopes.is_empty());
        assert!(creds2.token_received_at.is_none());

        // Round-trip through the current serializer and reload.
        let json = serde_json::to_string(&store).unwrap();
        let reloaded: McpCredentialStore = serde_json::from_str(&json).unwrap();
        let re = reloaded
            .get("linear", &url)
            .expect("round-trip keeps entry");
        assert_eq!(re.client_id, "legacy-client-id");
        let re_token = re.token_response.as_ref().expect("round-trip keeps token");
        assert_eq!(re_token.access_token().secret(), "at-123");
        assert_eq!(re_token.refresh_token().unwrap().secret(), "rt-456");
        assert_eq!(re.granted_scopes, vec!["read", "write"]);
        assert_eq!(re.token_received_at, Some(1730000000));
    }

    #[cfg(unix)]
    #[test]
    fn save_to_preserves_relative_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        let target = shared.join("mcp_credentials.json");
        std::fs::write(&target, "{}").unwrap();
        let link = dir.path().join("mcp_credentials.json");
        symlink("shared/mcp_credentials.json", &link).unwrap();

        let mut store = McpCredentialStore::default();
        let url = Url::parse("https://test.example.com/mcp").unwrap();
        store.insert_rmcp("test", &url, test_stored_creds("test-client"));
        store.save_to(&link).unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert!(McpCredentialStore::load_from(&target)
            .unwrap()
            .get("test", &url)
            .is_some());
    }

    #[test]
    fn save_and_load_from_file() {
        let dir = std::env::temp_dir().join("grok-mcp-credentials-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test_creds.json");

        let mut store = McpCredentialStore::default();
        let url = Url::parse("https://test.example.com/mcp").unwrap();
        store.insert_rmcp("test", &url, test_stored_creds("test-client"));
        store.save_to(&path).unwrap();

        let loaded = McpCredentialStore::load_from(&path).unwrap();
        assert!(loaded.get("test", &url).is_some());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn save_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let mut store = McpCredentialStore::default();
        let url = Url::parse("https://test.example.com/mcp").unwrap();
        store.insert_rmcp("test", &url, test_stored_creds("c"));
        store.save_to(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn load_tightens_world_readable_credentials() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let mut store = McpCredentialStore::default();
        let url = Url::parse("https://test.example.com/mcp").unwrap();
        store.insert_rmcp("test", &url, test_stored_creds("c"));
        store.save_to(&path).unwrap();
        let mut loose = std::fs::metadata(&path).unwrap().permissions();
        loose.set_mode(0o644);
        std::fs::set_permissions(&path, loose).unwrap();

        let _ = McpCredentialStore::load_from(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
