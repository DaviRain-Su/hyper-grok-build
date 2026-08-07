//! Provisioned-repo listing (`workspace.repos_list`) and the on-disk
//! in-sandbox manifest contract (`{workspace}/.grok/repos.json`).
//!
//! The sandbox provisioner writes this manifest; the workspace list op
//! reads it. Field names are the frontend/integration API — add optional
//! fields with `#[serde(default)]` rather than renaming existing ones.

use serde::{Deserialize, Serialize};

use super::WorkspaceRpc;

/// Relative path of the provisioner manifest from the **sandbox**
/// `workspace_directory` (pre-grove-rewrite init root, usually `/workspace`).
/// Not relative to agent / workspace-server `--cwd` after a single-repo grove
/// rewrite (`/workspace/app`). Writers and `workspace.repos_list` must join
/// this to that sandbox root.
pub const REPOS_MANIFEST_RELATIVE_PATH: &str = ".grok/repos.json";

/// Current on-disk / wire manifest version.
pub const REPOS_MANIFEST_VERSION: u32 = 1;

/// `workspace.repos_list` — list repos materialized into this workspace.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReposListReq {}

impl WorkspaceRpc for ReposListReq {
    const METHOD: &'static str = "workspace.repos_list";
    type Response = ReposListResponse;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReposListResponse {
    /// On-disk manifest version (`REPOS_MANIFEST_VERSION` for a missing file).
    #[serde(default)]
    pub version: u32,
    pub repos: Vec<ProvisionedRepo>,
}

/// One provisioned repository as exposed to frontend / workspace callers.
///
/// Expandable: new optional fields should use `#[serde(default, skip_serializing_if = "Option::is_none")]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProvisionedRepo {
    /// Short directory / display name (usually the git repo name).
    pub name: String,
    /// Repository identity (`owner/repo` or normalized URL form).
    pub repository: String,
    /// Absolute in-sandbox (or workspace-relative absolute) mount path.
    pub mount_path: String,
    /// Fork-from ref. Empty = unset (missing session branch is fatal).
    /// `"HEAD"` = remote default. Do not treat empty as HEAD.
    pub base_branch: String,
    /// Session working branch created at provision time.
    pub session_branch: String,
}

/// On-disk manifest written by the sandbox provisioner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoManifest {
    pub version: u32,
    pub repos: Vec<ProvisionedRepo>,
}

impl RepoManifest {
    pub fn new(repos: Vec<ProvisionedRepo>) -> Self {
        Self {
            version: REPOS_MANIFEST_VERSION,
            repos,
        }
    }

    /// Parse bytes from `{workspace}/.grok/repos.json`.
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    pub fn to_json_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_constant() {
        assert_eq!(ReposListReq::METHOD, "workspace.repos_list");
    }

    #[test]
    fn manifest_round_trip() {
        let manifest = RepoManifest::new(vec![
            ProvisionedRepo {
                name: "app".into(),
                repository: "acme/app".into(),
                mount_path: "/workspace/app".into(),
                base_branch: "main".into(),
                session_branch: "grok/s1".into(),
            },
            ProvisionedRepo {
                name: "lib".into(),
                repository: "acme/lib".into(),
                mount_path: "/workspace/lib".into(),
                base_branch: "HEAD".into(),
                session_branch: "feat/x".into(),
            },
        ]);
        let bytes = manifest.to_json_bytes().expect("serialize");
        let recovered = RepoManifest::from_json_bytes(&bytes).expect("parse");
        assert_eq!(manifest, recovered);
    }
}
