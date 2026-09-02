//! The single MCP policy verdict API. Nothing calls it in this PR; it is
//! the target surface the stacked enforcement PR migrates every consumer
//! onto (session merge, discovery, doctor, `mcp/list`, enable/upsert gates,
//! inspect, and the agent-level MCP pool), replacing each caller's own
//! deny/allow/pin reason assembly (e.g. the shell's `McpDisabledReason`).

use std::path::{Path, PathBuf};

use super::ManagedSettings;
use super::mcp::PolicySubjectOrigin;

/// Policy verdict for one MCP server definition.
#[derive(Debug, Clone)]
pub enum McpVerdict {
    Allowed,
    Blocked(McpBlockReason),
}

/// Why policy blocks an MCP server, attributed to the blocking source. The
/// `Display` strings are wire/UX payloads (pager rows, doctor details, enable
/// errors) — do not reword.
#[derive(Debug, Clone)]
pub enum McpBlockReason {
    /// Matches a `deniedMcpServers` entry.
    Deny { source: PathBuf },
    /// Missing from `allowedMcpServers` (or an active lockdown's grant list).
    NotGranted { source: PathBuf },
    /// Project-declared and not allowlisted under an
    /// `enableAllProjectMcpServers = false` pin.
    ProjectPin { source: PathBuf },
}

impl McpBlockReason {
    /// The policy source the block is attributed to.
    pub fn source(&self) -> &Path {
        match self {
            Self::Deny { source } | Self::NotGranted { source } | Self::ProjectPin { source } => {
                source
            }
        }
    }
}

impl std::fmt::Display for McpBlockReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Deny { source } => {
                write!(f, "matches deniedMcpServers ({})", source.display())
            }
            Self::NotGranted { source } => {
                write!(f, "not in allowedMcpServers ({})", source.display())
            }
            Self::ProjectPin { source } => {
                write!(
                    f,
                    "project MCP disabled (enableAllProjectMcpServers = false, {})",
                    source.display()
                )
            }
        }
    }
}

/// What defined the server, as policy sees it.
#[derive(Debug, Clone, Copy)]
pub struct McpSubject {
    pub origin: PolicySubjectOrigin,
    /// Declared by a project source (drives the project-MCP pin).
    pub project_scoped: bool,
}

impl ManagedSettings {
    /// The project-pin leg of [`Self::mcp_verdict`] in isolation: the
    /// `enableAllProjectMcpServers = false` pin blocks a project-scoped
    /// server with no allow-entry grant. The session merge (and discovery)
    /// apply this BEFORE the deny/allow verdict — a pinned-off server is
    /// dropped outright, never tagged — while `mcp_verdict` attributes
    /// deny/allow first for the reporting surfaces.
    pub fn mcp_project_pin_block(
        &self,
        server: &agent_client_protocol::McpServer,
        subject: McpSubject,
    ) -> Option<McpBlockReason> {
        let source = self.project_mcp.source()?;
        if !subject.project_scoped || self.mcp_allowlist.matches_allow_entry(server) {
            return None;
        }
        Some(McpBlockReason::ProjectPin {
            source: source.to_path_buf(),
        })
    }

    /// Policy verdict for `server` as defined by `subject`: any binding deny
    /// wins, then a missing allow grant (lockdown or a restricted source),
    /// then the project-MCP pin; otherwise allowed.
    pub fn mcp_verdict(
        &self,
        server: &agent_client_protocol::McpServer,
        subject: McpSubject,
    ) -> McpVerdict {
        let policy = &self.mcp_allowlist;
        if !policy.is_server_allowed(server, subject.origin) {
            if let Some(denying) = policy.denying_source(server, subject.origin) {
                return McpVerdict::Blocked(McpBlockReason::Deny {
                    source: denying.source_path.clone().unwrap_or_default(),
                });
            }
            let source = policy
                .blocking_allow_source(server, subject.origin)
                .and_then(|s| s.source_path.clone())
                .unwrap_or_default();
            return McpVerdict::Blocked(McpBlockReason::NotGranted { source });
        }
        if let Some(reason) = self.mcp_project_pin_block(server, subject) {
            return McpVerdict::Blocked(reason);
        }
        McpVerdict::Allowed
    }
}
