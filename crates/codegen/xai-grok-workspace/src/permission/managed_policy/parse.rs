//! Parsers for the managed policy keys (Claude camelCase JSON and grok
//! snake_case TOML-as-JSON): MCP allow/deny lists, marketplace lists, and
//! boolean pins.

use tracing::warn;

use super::layer::PolicyLayerOwnership;
use super::marketplace::{ManagedMarketplace, ManagedMarketplaceKind};
use super::mcp::AllowedMcpServer;
use super::url_match::{warn_on_unmatchable_allow_url, warn_on_unmatchable_deny_url};

/// Read a boolean policy key (camelCase or snake_case); non-bool values warn
/// with the spelling actually present in the source.
pub(super) fn policy_bool(json: &serde_json::Value, camel: &str, snake: &str) -> Option<bool> {
    let (key, value) = entry_field_with_key(json, camel, snake)?;
    let parsed = value.as_bool();
    if parsed.is_none() {
        warn!(key, %value, "policy key must be a boolean; ignoring");
    }
    parsed
}

/// `strictKnownMarketplaces` → allowlist URLs (`git`+`url` and `github`+`repo`;
/// `ref` tolerated). Unsupported entries warn — dropping silently would fail
/// open when an admin ships github-only entries.
pub(super) fn parse_strict_marketplaces(json: &serde_json::Value) -> Vec<String> {
    let Some(arr) = policy_array(json, "strictKnownMarketplaces", "strict_known_marketplaces")
    else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|entry| match marketplace_source_from_json(entry) {
            Some(ManagedMarketplaceKind::Git { url, .. }) => Some(url),
            Some(ManagedMarketplaceKind::Local { .. }) | None => {
                warn!(
                    entry = %entry,
                    "ignoring unsupported strictKnownMarketplaces entry; only git+url and github+repo sources are honored"
                );
                None
            }
        })
        .collect()
}

/// `extraKnownMarketplaces`: map of name → `{ source: {…}, autoUpdate? }`
/// (Claude shape); the per-entry `autoUpdate` opt-out rides along.
pub(super) fn parse_extra_marketplaces(
    json: &serde_json::Value,
    ownership: PolicyLayerOwnership,
) -> Vec<(ManagedMarketplace, Option<bool>)> {
    let Some((key, value)) =
        entry_field_with_key(json, "extraKnownMarketplaces", "extra_known_marketplaces")
    else {
        return Vec::new();
    };
    let Some(map) = value.as_object() else {
        warn!(key, %value, "policy key must be a table of marketplaces; the whole list is ignored");
        return Vec::new();
    };
    let mut out = Vec::new();
    for (name, entry) in map {
        let Some(kind) = entry.get("source").and_then(marketplace_source_from_json) else {
            warn!(
                name,
                "ignoring extraKnownMarketplaces entry without a supported source"
            );
            continue;
        };
        // `policy_bool` so a wrong-typed value warns instead of silently
        // dropping the auto-update opt-out.
        let auto_update = policy_bool(entry, "autoUpdate", "auto_update");
        out.push((
            ManagedMarketplace {
                name: name.clone(),
                kind,
                ownership,
            },
            auto_update,
        ));
    }
    out
}

/// One Claude source object: `{source:"git",url,ref?}`, `{source:"github",
/// repo,ref?}` (→ clone URL), or `{source:"local",path}`; `branch` ≡ `ref`.
fn marketplace_source_from_json(entry: &serde_json::Value) -> Option<ManagedMarketplaceKind> {
    let git_ref = entry
        .get("ref")
        .or_else(|| entry.get("branch"))
        .and_then(|v| v.as_str())
        .map(String::from);
    match entry.get("source")?.as_str()? {
        "git" => Some(ManagedMarketplaceKind::Git {
            url: entry.get("url")?.as_str()?.to_string(),
            git_ref,
        }),
        "github" => Some(ManagedMarketplaceKind::Git {
            url: format!("https://github.com/{}.git", entry.get("repo")?.as_str()?),
            git_ref,
        }),
        "local" => Some(ManagedMarketplaceKind::Local {
            path: entry.get("path")?.as_str()?.to_string(),
        }),
        _ => None,
    }
}

const ALLOWED_MCP_SERVERS_KEY: &str = "allowedMcpServers";
const DENIED_MCP_SERVERS_KEY: &str = "deniedMcpServers";

/// Which MCP policy list a source key names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum McpPolicyList {
    Allow,
    Deny,
}

impl McpPolicyList {
    fn camel(self) -> &'static str {
        match self {
            Self::Allow => ALLOWED_MCP_SERVERS_KEY,
            Self::Deny => DENIED_MCP_SERVERS_KEY,
        }
    }

    fn snake(self) -> &'static str {
        match self {
            Self::Allow => "allowed_mcp_servers",
            Self::Deny => "denied_mcp_servers",
        }
    }
}

/// Managed-policy config keys (both spellings), consumed here — the shell's
/// unknown-key check exempts them (`is_non_serde_config_path`), and
/// [`resolve_managed_settings`] reads only these keys out of each TOML layer.
pub const MANAGED_POLICY_CONFIG_KEYS: &[&str] = &[
    "allowedMcpServers",
    "allowed_mcp_servers",
    "deniedMcpServers",
    "denied_mcp_servers",
    "allowManagedMcpServersOnly",
    "allow_managed_mcp_servers_only",
    "enableAllProjectMcpServers",
    "enable_all_project_mcp_servers",
    "pluginAutoUpdate",
    "plugin_auto_update",
    "strictKnownMarketplaces",
    "strict_known_marketplaces",
    "extraKnownMarketplaces",
    "extra_known_marketplaces",
];

/// Read a field by its Claude camelCase or TOML snake_case key.
fn entry_field<'a>(
    entry: &'a serde_json::Value,
    camel: &str,
    snake: &str,
) -> Option<&'a serde_json::Value> {
    entry_field_with_key(entry, camel, snake).map(|(_, value)| value)
}

/// [`entry_field`] returning the spelling that matched (for diagnostics).
fn entry_field_with_key<'a, 'k>(
    entry: &'a serde_json::Value,
    camel: &'k str,
    snake: &'k str,
) -> Option<(&'k str, &'a serde_json::Value)> {
    entry
        .get(camel)
        .map(|value| (camel, value))
        .or_else(|| entry.get(snake).map(|value| (snake, value)))
}

/// Read an array policy key (camelCase or snake_case). A present but
/// non-array value warns — TOML admins write `key = { … }` or a bare string
/// easily, and silently parsing that as an empty list is zero enforcement.
fn policy_array<'a>(
    json: &'a serde_json::Value,
    camel: &str,
    snake: &str,
) -> Option<&'a Vec<serde_json::Value>> {
    let (key, value) = entry_field_with_key(json, camel, snake)?;
    let parsed = value.as_array();
    if parsed.is_none() {
        warn!(key, %value, "policy key must be an array of entries; the whole list is ignored");
    }
    parsed
}

/// Parse one allow/deny list (camelCase or snake_case key).
pub(super) fn parse_mcp_entry_list(
    json: &serde_json::Value,
    list: McpPolicyList,
) -> Vec<AllowedMcpServer> {
    let Some(arr) = policy_array(json, list.camel(), list.snake()) else {
        return Vec::new();
    };
    parse_mcp_entries(arr, list)
}

/// Parse `serverUrl` → Http, string `command` → Stdio, `serverCommand` array →
/// StdioArgv, `serverName` → Name (snake_case accepted). Unsupported deny
/// entries `warn!` (silent drop = zero enforcement); the allow side stays
/// silent (ungranted = fail-closed).
fn parse_mcp_entries(arr: &[serde_json::Value], list: McpPolicyList) -> Vec<AllowedMcpServer> {
    let is_deny = list == McpPolicyList::Deny;
    let mut entries = Vec::new();
    for entry in arr {
        if let Some(url) = entry_field(entry, "serverUrl", "server_url").and_then(|u| u.as_str()) {
            if is_deny {
                warn_on_unmatchable_deny_url(url);
            } else {
                warn_on_unmatchable_allow_url(url);
            }
            entries.push(AllowedMcpServer::Http {
                url_pattern: url.to_string(),
            });
        } else if let Some(argv) = entry_field(entry, "serverCommand", "server_command")
            .and_then(|c| c.as_array())
            // All-or-nothing: a partial argv would match the wrong command.
            .and_then(|a| {
                a.iter()
                    .map(|v| v.as_str().map(String::from))
                    .collect::<Option<Vec<_>>>()
            })
            .filter(|argv| !argv.is_empty())
        {
            entries.push(AllowedMcpServer::StdioArgv { argv });
        } else if let Some(cmd) = entry.get("command").and_then(|c| c.as_str()) {
            entries.push(AllowedMcpServer::Stdio {
                command: cmd.to_string(),
            });
        } else if let Some(name) =
            entry_field(entry, "serverName", "server_name").and_then(|n| n.as_str())
        {
            entries.push(AllowedMcpServer::Name {
                name: name.to_string(),
            });
        } else if is_deny {
            warn!(
                entry = %entry,
                "ignoring unsupported deniedMcpServers entry; only serverUrl, command, serverCommand, and serverName are honored"
            );
        }
    }
    entries
}
