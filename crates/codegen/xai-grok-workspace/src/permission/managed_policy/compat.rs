//! Transitional shims for pre-origin-aware callers; deleted in the
//! enforcement PR stacked on this one. Reproduces the old single-Claude-
//! source, origin-unaware `resolution` surface: MCP matching delegates to
//! the engine (the matchers moved verbatim); marketplace normalization and
//! parsing are old-code copies that deliberately differ from the engine's.

use std::path::PathBuf;
use std::sync::OnceLock;

use super::mcp::PolicySubjectOrigin;
use super::{ManagedSettingsFeatures, McpServerPolicy};

/// The old 3-variant entry shape (no `StdioArgv`; old callers match
/// exhaustively).
#[derive(Debug, Clone)]
pub enum AllowedMcpServer {
    Http {
        url_pattern: String,
    },
    Stdio {
        command: String,
    },
    /// Match by config name (any transport).
    Name {
        name: String,
    },
}

impl AllowedMcpServer {
    fn to_engine(&self) -> super::AllowedMcpServer {
        match self {
            Self::Http { url_pattern } => super::AllowedMcpServer::Http {
                url_pattern: url_pattern.clone(),
            },
            Self::Stdio { command } => super::AllowedMcpServer::Stdio {
                command: command.clone(),
            },
            Self::Name { name } => super::AllowedMcpServer::Name { name: name.clone() },
        }
    }
}

/// The old single-source MCP policy: deny beats allow, allow is a
/// per-dimension union, a deny-only policy allows the rest.
#[derive(Debug, Clone, Default)]
pub struct McpServerAllowlist {
    pub entries: Vec<AllowedMcpServer>,
    pub deny_entries: Vec<AllowedMcpServer>,
    pub source_path: Option<PathBuf>,
    /// The same entries as a single-source engine policy. Evaluated with a
    /// binding origin and no managed-only pin, the engine reproduces the old
    /// origin-unaware semantics exactly.
    policy: McpServerPolicy,
}

impl McpServerAllowlist {
    pub fn new(
        entries: Vec<AllowedMcpServer>,
        deny_entries: Vec<AllowedMcpServer>,
        source_path: Option<PathBuf>,
    ) -> Self {
        let policy = McpServerPolicy::single(super::McpServerAllowlist::new(
            entries.iter().map(AllowedMcpServer::to_engine).collect(),
            deny_entries
                .iter()
                .map(AllowedMcpServer::to_engine)
                .collect(),
            source_path.clone(),
        ));
        Self {
            entries,
            deny_entries,
            source_path,
            policy,
        }
    }

    pub fn is_restricted(&self) -> bool {
        !self.entries.is_empty() || !self.deny_entries.is_empty()
    }

    pub fn is_server_allowed(&self, server: &agent_client_protocol::McpServer) -> bool {
        self.policy
            .is_server_allowed(server, PolicySubjectOrigin::Foreign)
    }

    pub fn is_server_denied(&self, server: &agent_client_protocol::McpServer) -> bool {
        self.policy
            .is_server_denied(server, PolicySubjectOrigin::Foreign)
    }
}

/// The old single-source marketplace allowlist.
#[derive(Debug, Clone, Default)]
pub struct MarketplaceAllowlist {
    pub allowed_urls: Vec<String>,
    pub source_path: Option<PathBuf>,
}

impl MarketplaceAllowlist {
    pub fn is_restricted(&self) -> bool {
        !self.allowed_urls.is_empty()
    }

    pub fn is_url_allowed(&self, url: &str) -> bool {
        if self.allowed_urls.is_empty() {
            return true;
        }
        let normalized = normalize_git_url(url);
        self.allowed_urls
            .iter()
            .any(|allowed| normalize_git_url(allowed) == normalized)
    }

    pub fn block_reason(&self) -> String {
        match &self.source_path {
            Some(p) => format!("source not in strictKnownMarketplaces ({})", p.display()),
            None => "source not in strictKnownMarketplaces".to_string(),
        }
    }
}

/// The old normalization: case-fold the whole URL (path included) and strip
/// every trailing `.git` — deliberately NOT the engine's
/// [`super::normalize_git_url`], which keeps repo paths case-sensitive.
fn normalize_git_url(url: &str) -> String {
    url.to_lowercase().trim_end_matches(".git").to_string()
}

/// The old `managed_settings()` view: features from the engine (unchanged
/// semantics), MCP/marketplace allowlists parsed from the single Claude
/// `managed-settings.json` exactly as the old loader did. The engine's TOML
/// policy layers are invisible here — multi-source enforcement lands with
/// the migrated callers in the stacked PR.
#[derive(Debug, Default)]
pub struct ManagedSettings {
    pub features: ManagedSettingsFeatures,
    pub mcp_allowlist: McpServerAllowlist,
    pub marketplace_allowlist: MarketplaceAllowlist,
}

static COMPAT_MANAGED_SETTINGS: OnceLock<ManagedSettings> = OnceLock::new();

pub fn managed_settings() -> &'static ManagedSettings {
    COMPAT_MANAGED_SETTINGS.get_or_init(load_managed_settings)
}

fn load_managed_settings() -> ManagedSettings {
    let engine = super::managed_settings();
    let features = ManagedSettingsFeatures {
        disable_telemetry: engine.features.disable_telemetry,
        disable_feedback: engine.features.disable_feedback,
        disable_yolo: engine.features.disable_yolo,
        source_path: engine.features.source_path.clone(),
    };
    let Some(path) = xai_grok_config::claude_managed_settings_path() else {
        return ManagedSettings {
            features,
            ..Default::default()
        };
    };
    let Some(json) = super::read_managed_settings_json(&path) else {
        return ManagedSettings {
            features,
            ..Default::default()
        };
    };
    ManagedSettings {
        features,
        mcp_allowlist: McpServerAllowlist::new(
            parse_mcp_entries(&json, "allowedMcpServers"),
            parse_mcp_entries(&json, "deniedMcpServers"),
            Some(path.clone()),
        ),
        marketplace_allowlist: MarketplaceAllowlist {
            allowed_urls: parse_marketplace_urls(&json),
            source_path: Some(path),
        },
    }
}

/// The old parser: `serverUrl` → Http, `command` → Stdio, `serverName` →
/// Name; anything else dropped. Known one-PR gap: a `serverCommand`-shaped
/// deny entry drops with no diagnostic (the old parser warned; the engine
/// supports the shape, so it won't).
fn parse_mcp_entries(json: &serde_json::Value, key: &str) -> Vec<AllowedMcpServer> {
    let Some(arr) = json.get(key).and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for entry in arr {
        if let Some(url) = entry.get("serverUrl").and_then(|u| u.as_str()) {
            entries.push(AllowedMcpServer::Http {
                url_pattern: url.to_string(),
            });
        } else if let Some(cmd) = entry.get("command").and_then(|c| c.as_str()) {
            entries.push(AllowedMcpServer::Stdio {
                command: cmd.to_string(),
            });
        } else if let Some(name) = entry.get("serverName").and_then(|n| n.as_str()) {
            entries.push(AllowedMcpServer::Name {
                name: name.to_string(),
            });
        }
    }
    entries
}

/// The old `strictKnownMarketplaces` parse: git-source URLs only.
fn parse_marketplace_urls(json: &serde_json::Value) -> Vec<String> {
    json.get("strictKnownMarketplaces")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|entry| {
                    let source = entry.get("source")?.as_str()?;
                    if source != "git" {
                        return None;
                    }
                    entry.get("url").and_then(|u| u.as_str()).map(String::from)
                })
                .collect()
        })
        .unwrap_or_default()
}
