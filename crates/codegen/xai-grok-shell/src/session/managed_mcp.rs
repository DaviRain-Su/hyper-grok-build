//! Shell-side managed MCP: merges MCP server sources, then injects managed
//! OAuth headers, and binds the extracted credential/catalog machinery to
//! shell's auth manager.
//!
//! Merge layers are applied in order; later `insert()` beats earlier
//! `or_insert()`:
//!   - config.toml    — seeds the map; `enabled = false` blocks lower layers
//!   - Plugins        — `or_insert` (won't override config.toml)
//!   - ~/.claude.json — `or_insert` (imported user/local MCP servers)
//!   - `.mcp.json`    — `or_insert` (team baseline)
//!   - Client         — `insert` (always wins)
//!   - Managed        — header injection + auto-create missing connectors
//!
//! The transport/cache/injection core lives in
//! `xai_grok_shell_session_support::managed_mcp` and is re-exported here so
//! `crate::session::managed_mcp::…` paths keep resolving unchanged.

pub use xai_grok_shell_session_support::managed_mcp::*;

use std::collections::HashMap;
use std::sync::Arc;

use agent_client_protocol as acp;

/// Build a [`RefreshContext`] whose token provider resolves fresh tokens from
/// `auth_manager`; the extracted refresh task never sees the auth manager
/// itself, only the closure.
fn refresh_context(
    proxy_base_url: String,
    auth_manager: Arc<crate::auth::AuthManager>,
) -> RefreshContext {
    RefreshContext {
        proxy_base_url,
        token_provider: Arc::new(move || -> TokenFuture {
            let auth_manager = auth_manager.clone();
            Box::pin(async move { auth_manager.get_valid_token().await.ok() })
        }),
    }
}

/// Resolve an auth key from `auth_manager` then [`get_or_fetch`] the managed MCP
/// configs (with a [`RefreshContext`] for proactive refresh). Single source for
/// the auth-key dance across every managed-config fetch —
/// [`crate::agent::MvpAgent::get_managed_mcp_configs`], the interactive
/// folder-trust grant reload, agent-init MCP setup, and the reactive re-auth
/// re-fetch — so the copies can't drift.
/// Callers gate on `can_fetch_managed_mcps`/auth before calling.
pub(crate) async fn fetch_managed_mcp_configs(
    handle: &ManagedMcpStateHandle,
    proxy_url: &str,
    auth_manager: &Arc<crate::auth::AuthManager>,
) -> Vec<ManagedMcpConfig> {
    let auth_key = auth_manager
        .get_valid_token()
        .await
        .ok()
        .or_else(|| auth_manager.current_or_expired().map(|a| a.key));
    get_or_fetch(
        handle,
        proxy_url,
        auth_key.as_deref(),
        Some(refresh_context(proxy_url.to_string(), auth_manager.clone())),
    )
    .await
}

/// Dedup key for the merge map: normalized URL for Http/Sse, name for Stdio.
fn mcp_server_key(s: &acp::McpServer) -> String {
    match s {
        acp::McpServer::Http(acp::McpServerHttp { url, .. })
        | acp::McpServer::Sse(acp::McpServerSse { url, .. }) => normalize_url(url),
        acp::McpServer::Stdio(acp::McpServerStdio { name, .. }) => name.clone(),
        // TODO(acp-0.10): `McpServer` is #[non_exhaustive].
        _ => String::new(),
    }
}

pub(crate) fn mcp_server_name(s: &acp::McpServer) -> &str {
    match s {
        acp::McpServer::Http(acp::McpServerHttp { name, .. })
        | acp::McpServer::Sse(acp::McpServerSse { name, .. })
        | acp::McpServer::Stdio(acp::McpServerStdio { name, .. }) => name,
        // TODO(acp-0.10): `McpServer` is #[non_exhaustive].
        _ => "",
    }
}

pub fn merge_managed_mcp_servers(
    client_mcp_servers: Vec<acp::McpServer>,
    cwd: &std::path::Path,
    managed_configs: &[ManagedMcpConfig],
    plugin_registry: Option<&xai_grok_agent::plugins::PluginRegistry>,
    compat: &xai_grok_tools::types::compat::CompatConfig,
) -> Vec<acp::McpServer> {
    merge_managed_mcp_servers_with_policy(
        client_mcp_servers,
        cwd,
        managed_configs,
        plugin_registry,
        compat,
    )
    .into_iter()
    .filter(|s| s.disabled_reason.is_none())
    .map(|s| s.server)
    .collect()
}

/// Merge the managed catalog into ONE live session's MCP set and push the
/// result via [`crate::session::SessionCommand::UpdateMcpServers`]; returns
/// `true` if the command was enqueued (session still alive).
///
/// Shared core for every "re-merge managed configs into a live session" path
/// (`mcp/list cache=false`, config hot-reload, post-grant reload) so the merge
/// inputs and the dropped-oneshot-response contract can't drift between them.
pub(crate) fn merge_and_send_managed_mcp_update(
    cmd_tx: &tokio::sync::mpsc::UnboundedSender<crate::session::SessionCommand>,
    cwd: &std::path::Path,
    initial_client_mcp_servers: Vec<acp::McpServer>,
    managed: &[ManagedMcpConfig],
    plugin_registry: Option<&xai_grok_agent::plugins::PluginRegistry>,
    compat: &xai_grok_tools::types::compat::CompatConfig,
) -> bool {
    let merged = merge_managed_mcp_servers(
        initial_client_mcp_servers,
        cwd,
        managed,
        plugin_registry,
        compat,
    );
    let (tx, _rx) = tokio::sync::oneshot::channel();
    cmd_tx
        .send(crate::session::SessionCommand::UpdateMcpServers {
            mcp_servers: merged,
            respond_to: tx,
        })
        .is_ok()
}

pub fn merge_managed_mcp_servers_with_policy(
    client_mcp_servers: Vec<acp::McpServer>,
    cwd: &std::path::Path,
    managed_configs: &[ManagedMcpConfig],
    plugin_registry: Option<&xai_grok_agent::plugins::PluginRegistry>,
    compat: &xai_grok_tools::types::compat::CompatConfig,
) -> Vec<McpServerWithPolicy> {
    let mut servers: HashMap<String, acp::McpServer> =
        merge_managed_mcp_servers_sourced(cwd, plugin_registry, compat)
            .into_iter()
            .map(|(s, _source)| (mcp_server_key(&s), s))
            .collect();

    for server in client_mcp_servers {
        servers.insert(mcp_server_key(&server), server);
    }

    let disabled = crate::util::config::disabled_mcp_server_names(cwd);

    let mut merged: Vec<acp::McpServer> = servers.into_values().collect();
    inject_managed_headers(&mut merged, managed_configs);
    auto_inject_managed_servers_with_disabled(&mut merged, managed_configs, &disabled);
    // Deterministic order: this list is collected from a HashMap (random
    // iteration order). Downstream equality checks (`mcp_servers_equal`, used
    // by both `update_configs` and the `update_configs_diff` short-circuit) are
    // order-sensitive, so an unsorted list makes an unchanged server set look
    // changed — spuriously cancelling/restarting MCP init on e.g. a hooks-only
    // plugin reload. Sorting by the dedup key keeps reloads a true no-op when
    // nothing changed.
    merged.sort_by_key(mcp_server_key);
    // Folder-trust gate: when `cwd`'s workspace is untrusted, drop its
    // repo-local (project-scoped) servers before they can be spawned. No-op for
    // a trusted/unrecorded workspace. Composes with the managed-deny allowlist
    // applied next (both filters run on the survivors).
    let merged = crate::agent::folder_trust::filter_untrusted_project_mcp(cwd, merged);
    let allowlist = &xai_grok_workspace::permission::resolution::managed_settings().mcp_allowlist;
    apply_mcp_server_policy(merged, &disabled, allowlist)
}

/// Why an MCP server was disabled by policy.
#[derive(Debug, Clone)]
pub enum McpDisabledReason {
    Allowlist { source: std::path::PathBuf },
    Denylist { source: std::path::PathBuf },
}

impl std::fmt::Display for McpDisabledReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Allowlist { source } => {
                write!(f, "not in allowedMcpServers ({})", source.display())
            }
            Self::Denylist { source } => {
                write!(f, "matches deniedMcpServers ({})", source.display())
            }
        }
    }
}

impl McpDisabledReason {
    /// Classify why a blocked server was rejected by the managed-settings
    /// MCP policy: an explicit deny match vs a missing allowlist entry.
    pub fn for_blocked_server(
        policy: &xai_grok_workspace::permission::resolution::McpServerAllowlist,
        server: &acp::McpServer,
    ) -> Self {
        let source = policy.source_path.clone().unwrap_or_default();
        if policy.is_server_denied(server) {
            Self::Denylist { source }
        } else {
            Self::Allowlist { source }
        }
    }
}

/// An MCP server paired with its policy status.
pub struct McpServerWithPolicy {
    pub server: acp::McpServer,
    pub disabled_reason: Option<McpDisabledReason>,
}

/// Tag each merged MCP server with its managed-settings policy status and drop
/// names disabled in config.toml. A server that fails `is_server_allowed` is
/// tagged with a deny-vs-allowlist `McpDisabledReason` (via
/// `McpDisabledReason::for_blocked_server`); the public
/// `merge_managed_mcp_servers` then drops every tagged server. Split out from
/// `merge_managed_mcp_servers_with_policy` so the deny/allow enforcement
/// chokepoint can be tested with an injected allowlist — the runtime path reads
/// the process-wide managed-settings `OnceLock`, which a test can't populate.
fn apply_mcp_server_policy(
    merged: Vec<acp::McpServer>,
    disabled: &std::collections::HashSet<String>,
    allowlist: &xai_grok_workspace::permission::resolution::McpServerAllowlist,
) -> Vec<McpServerWithPolicy> {
    merged
        .into_iter()
        .filter_map(|server| {
            if disabled.contains(mcp_server_name(&server)) {
                return None;
            }
            if !allowlist.is_server_allowed(&server) {
                let reason = McpDisabledReason::for_blocked_server(allowlist, &server);
                tracing::warn!(
                    name = mcp_server_name(&server),
                    reason = %reason,
                    "MCP server blocked by managed settings policy"
                );
                return Some(McpServerWithPolicy {
                    server,
                    disabled_reason: Some(reason),
                });
            }
            Some(McpServerWithPolicy {
                server,
                disabled_reason: None,
            })
        })
        .collect()
}

/// Like [`merge_managed_mcp_servers`] but returns `ConfigSource` alongside each server.
pub fn merge_managed_mcp_servers_sourced(
    cwd: &std::path::Path,
    plugin_registry: Option<&xai_grok_agent::plugins::PluginRegistry>,
    compat: &xai_grok_tools::types::compat::CompatConfig,
) -> Vec<(
    acp::McpServer,
    xai_grok_tools::types::config_source::ConfigSource,
)> {
    let _mcp_merge_timer = crate::instrumentation::timer("mcp_merge_managed");
    use xai_grok_tools::types::config_source::ConfigSource;

    let toml_claimed_names = crate::util::config::all_toml_mcp_server_names(cwd);

    let config_source = ConfigSource::ConfigToml {
        path: xai_grok_tools::util::grok_home::grok_home().join("config.toml"),
    };

    // Use the TOML-only loader so that entries from imported editor configs
    // and .mcp.json are not pre-loaded with ConfigSource::ConfigToml.  Those
    // sources are added below with their correct ConfigSource variants.
    let mut servers: HashMap<String, (acp::McpServer, ConfigSource)> =
        crate::util::config::load_mcp_servers_toml_only(cwd)
            .into_iter()
            .map(|s| {
                let key = mcp_server_key(&s);
                (key, (s, config_source.clone()))
            })
            .collect();
    for (name, (_, source)) in &servers {
        tracing::info!(server = name, source = ?source, "MCP server loaded from source");
    }

    // Plugins
    if let Some(registry) = plugin_registry {
        for plugin in registry.active_plugins() {
            let mut plugin_servers: Vec<acp::McpServer> = Vec::new();
            if let Some(ref mcp_path) = plugin.mcp_config_path {
                let (servers, _) = load_plugin_mcp_servers(
                    mcp_path,
                    &plugin.name,
                    &plugin.root_str(),
                    &plugin.data_dir_str(),
                );
                plugin_servers.extend(servers);
            }
            if let Some(ref inline_value) = plugin.inline_mcp_servers {
                let (servers, _) = load_plugin_mcp_servers_from_value(
                    inline_value,
                    &plugin.name,
                    &plugin.root_str(),
                    &plugin.data_dir_str(),
                );
                plugin_servers.extend(servers);
            }
            if plugin_servers.is_empty() {
                continue;
            }
            let mut seen_names: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            plugin_servers.retain(|server| seen_names.insert(mcp_server_name(server).to_string()));
            let source = ConfigSource::Plugin {
                plugin_name: plugin.name.clone(),
                path: plugin.root.clone(),
            };
            for server in plugin_servers {
                if toml_claimed_names.contains(mcp_server_name(&server)) {
                    continue;
                }
                let key = mcp_server_key(&server);
                servers.entry(key).or_insert((server, source.clone()));
            }
        }
    }

    // ~/.claude.json
    let claude_json_source = ConfigSource::ClaudeJson {
        path: dirs::home_dir()
            .map(|h| h.join(".claude.json"))
            .unwrap_or_default(),
    };
    for server in crate::util::config::load_claude_json_mcp_servers(cwd, compat) {
        if toml_claimed_names.contains(mcp_server_name(&server)) {
            continue;
        }
        let key = mcp_server_key(&server);
        servers
            .entry(key)
            .or_insert((server, claude_json_source.clone()));
    }

    // ~/.cursor/mcp.json
    let cursor_mcp_source = ConfigSource::McpJson {
        path: dirs::home_dir()
            .map(|h| h.join(".cursor").join("mcp.json"))
            .unwrap_or_default(),
    };
    for server in crate::util::config::load_cursor_mcp_servers(cwd, compat) {
        if toml_claimed_names.contains(mcp_server_name(&server)) {
            continue;
        }
        let key = mcp_server_key(&server);
        servers
            .entry(key)
            .or_insert((server, cursor_mcp_source.clone()));
    }

    // .mcp.json
    let mcp_json_source = ConfigSource::McpJson {
        path: cwd.join(".mcp.json"),
    };
    for server in crate::util::config::load_mcp_json_servers(cwd) {
        if toml_claimed_names.contains(mcp_server_name(&server)) {
            continue;
        }
        let key = mcp_server_key(&server);
        servers
            .entry(key)
            .or_insert((server, mcp_json_source.clone()));
    }

    servers.into_values().collect()
}

/// Auto-create `grok_com_*` entries for managed configs not already in `merged`.
/// Dedup by display name (first scope wins). Skips names in `disabled_names`.
pub(crate) fn auto_inject_managed_servers_with_disabled(
    merged: &mut Vec<acp::McpServer>,
    managed_configs: &[ManagedMcpConfig],
    disabled_names: &std::collections::HashSet<String>,
) {
    if managed_configs.is_empty() {
        return;
    }

    let existing_names: std::collections::HashSet<String> = merged
        .iter()
        .map(|s| mcp_server_name(s).to_owned())
        .collect();
    let mut seen_display_names: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut count = 0usize;

    for config in managed_configs {
        if config.headers.is_empty() {
            continue;
        }
        let name = to_managed_name(&config.name);

        if existing_names.contains(&name) {
            continue;
        }
        if disabled_names.contains(&name) {
            tracing::debug!(server_name = %name, "Auto-inject skipped: disabled in config.toml");
            continue;
        }
        if !seen_display_names.insert(config.name.to_lowercase()) {
            continue;
        }

        let headers = config
            .headers
            .iter()
            .map(|(k, v)| acp::HttpHeader::new(k.clone(), v.clone()))
            .collect();

        merged.push(acp::McpServer::Http(
            acp::McpServerHttp::new(name, config.endpoint.clone()).headers(headers),
        ));
        count += 1;
    }

    if count > 0 {
        tracing::info!(count, "Auto-injected managed MCP connectors");
    }
}

fn load_plugin_mcp_servers(
    mcp_path: &std::path::Path,
    plugin_name: &str,
    plugin_root: &str,
    plugin_data: &str,
) -> (Vec<acp::McpServer>, crate::util::config::McpOAuthConfigMap) {
    let Some(config) = crate::util::config::read_mcp_json(mcp_path) else {
        return (vec![], crate::util::config::McpOAuthConfigMap::new());
    };
    load_plugin_mcp_servers_from_config(&config, plugin_name, plugin_root, plugin_data)
}

/// Like [`load_plugin_mcp_servers`] but from an in-memory JSON value (no I/O).
fn load_plugin_mcp_servers_from_value(
    root: &serde_json::Value,
    plugin_name: &str,
    plugin_root: &str,
    plugin_data: &str,
) -> (Vec<acp::McpServer>, crate::util::config::McpOAuthConfigMap) {
    let normalized = xai_grok_agent::plugins::manifest::normalize_inline_mcp_servers(root);
    let Ok(config) = serde_json::from_value::<crate::util::config::McpConfig>(normalized) else {
        tracing::warn!(plugin = plugin_name, "failed to parse plugin MCP config");
        return (vec![], crate::util::config::McpOAuthConfigMap::new());
    };
    load_plugin_mcp_servers_from_config(&config, plugin_name, plugin_root, plugin_data)
}

fn load_plugin_mcp_servers_from_config(
    config: &crate::util::config::McpConfig,
    plugin_name: &str,
    plugin_root: &str,
    plugin_data: &str,
) -> (Vec<acp::McpServer>, crate::util::config::McpOAuthConfigMap) {
    let sub = |s: &str| -> String {
        let s = xai_grok_agent::plugins::manifest::substitute_env_vars(s, plugin_root, plugin_data);
        crate::config::expand_env_vars_in_string(&s)
    };
    let label = format!("plugin:{}", plugin_name);
    crate::util::config::parse_mcp_config_with_oauth(config, &label, &sub)
}

pub fn collect_plugin_oauth_configs(
    plugin_registry: Option<&xai_grok_agent::plugins::PluginRegistry>,
) -> crate::util::config::McpOAuthConfigMap {
    let mut oauth_configs = crate::util::config::McpOAuthConfigMap::new();
    let Some(registry) = plugin_registry else {
        return oauth_configs;
    };

    for plugin in registry.active_plugins() {
        if let Some(ref mcp_path) = plugin.mcp_config_path {
            let (_, oauth) = load_plugin_mcp_servers(
                mcp_path,
                &plugin.name,
                &plugin.root_str(),
                &plugin.data_dir_str(),
            );
            for (name, cfg) in oauth {
                oauth_configs.entry(name).or_insert(cfg);
            }
        }
        if let Some(ref inline_value) = plugin.inline_mcp_servers {
            let (_, oauth) = load_plugin_mcp_servers_from_value(
                inline_value,
                &plugin.name,
                &plugin.root_str(),
                &plugin.data_dir_str(),
            );
            for (name, cfg) in oauth {
                oauth_configs.entry(name).or_insert(cfg);
            }
        }
    }

    oauth_configs
}

pub fn merge_plugin_oauth_into(
    oauth_config_map: &mut crate::util::config::McpOAuthConfigMap,
    plugin_oauth: crate::util::config::McpOAuthConfigMap,
    toml_mcp_names: &std::collections::HashSet<String>,
) {
    for (name, cfg) in plugin_oauth {
        if toml_mcp_names.contains(&name) {
            continue;
        }
        oauth_config_map.insert(name, cfg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_managed(name: &str, endpoint: &str, scope: &str) -> ManagedMcpConfig {
        ManagedMcpConfig {
            name: name.to_string(),
            endpoint: endpoint.to_string(),
            headers: HashMap::from([("Authorization".into(), "Bearer tok".into())]),
            token_expires_at: None,
            scope: Some(scope.to_string()),
            scope_id: Some(format!("{scope}-id-123")),
            scope_name: None,
        }
    }

    fn empty_cwd() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn auto_inject_creates_server_for_unmatched_managed_config() {
        let managed = vec![make_managed("Slack", "https://mcp.slack.com/sse", "user")];
        let cwd = empty_cwd();
        let compat = xai_grok_tools::types::compat::CompatConfig::default();
        let merged = merge_managed_mcp_servers(vec![], cwd.path(), &managed, None, &compat);
        let slack = merged
            .iter()
            .find(|s| matches!(s, acp::McpServer::Http(acp::McpServerHttp { name, .. }) if name == "grok_com_slack"));
        let slack = slack.expect("should have auto-injected grok_com_slack");
        match slack {
            acp::McpServer::Http(acp::McpServerHttp { url, headers, .. }) => {
                assert_eq!(url, "https://mcp.slack.com/sse");
                assert!(headers.iter().any(|h| h.name == "Authorization"));
            }
            other => panic!("expected Http server, got {:?}", other),
        }
    }

    /// A client-provided server (e.g. a client session binding injected at
    /// `session/new`) exists in no on-disk config and no managed catalog —
    /// the merge must keep it. Config hot-reload handlers
    /// (`reload_all_mcp_servers` / `reload_project_mcp_servers`) rely on this
    /// by re-seeding the merge with the session's
    /// `initial_client_mcp_servers`; if this property breaks, those reloads
    /// silently tear down client-injected servers mid-session.
    #[test]
    fn client_provided_servers_survive_merge() {
        let client = vec![acp::McpServer::Http(
            acp::McpServerHttp::new(
                "demo-mcp".to_string(),
                "http://mcp.example.test/api/mcp".to_string(),
            )
            .headers(vec![]),
        )];
        let cwd = empty_cwd();
        let compat = xai_grok_tools::types::compat::CompatConfig::default();
        let merged = merge_managed_mcp_servers(client, cwd.path(), &[], None, &compat);
        assert!(
            merged.iter().any(|s| matches!(
                s,
                acp::McpServer::Http(acp::McpServerHttp { name, .. }) if name == "demo-mcp"
            )),
            "client-provided server must survive a merge with no disk/managed sources"
        );
    }

    /// The merge chokepoint must actually DROP a server matching
    /// `deniedMcpServers`, and classify the drop as a `Denylist` hit (not a
    /// missing `allowlist` entry) — that reason is the user-visible payload for
    /// the toggle error and `mcp doctor` detail. The runtime merge reads the
    /// process-wide managed-settings `OnceLock`, so we exercise the extracted
    /// `apply_mcp_server_policy` seam with an injected allowlist built via the
    /// public `McpServerAllowlist::new`.
    #[test]
    fn merge_drops_denied_server_and_classifies_as_denylist() {
        use xai_grok_workspace::permission::resolution::{AllowedMcpServer, McpServerAllowlist};

        // Deny-only policy (no allowlist) blocking one host.
        let allowlist = McpServerAllowlist::new(
            vec![],
            vec![AllowedMcpServer::Http {
                url_pattern: "https://blocked.corp.com/*".into(),
            }],
            Some(std::path::PathBuf::from("/test/managed-settings.json")),
        );

        let tagged = apply_mcp_server_policy(
            vec![
                acp::McpServer::Http(
                    acp::McpServerHttp::new("blocked", "https://blocked.corp.com/mcp")
                        .headers(vec![]),
                ),
                acp::McpServer::Http(
                    acp::McpServerHttp::new("ok", "https://ok.corp.com/mcp").headers(vec![]),
                ),
            ],
            &std::collections::HashSet::new(),
            &allowlist,
        );

        // Denied server is classified as a denylist hit, not a missing-allow.
        let blocked = tagged
            .iter()
            .find(|s| mcp_server_name(&s.server) == "blocked")
            .expect("denied server present in policy output");
        assert!(
            matches!(
                blocked.disabled_reason,
                Some(McpDisabledReason::Denylist { .. })
            ),
            "expected Denylist reason, got {:?}",
            blocked.disabled_reason
        );

        // Non-denied server passes untouched.
        let ok = tagged
            .iter()
            .find(|s| mcp_server_name(&s.server) == "ok")
            .expect("allowed server present in policy output");
        assert!(ok.disabled_reason.is_none());

        // The public `merge_managed_mcp_servers` drop predicate removes exactly
        // the denied server.
        let surviving: Vec<&str> = tagged
            .iter()
            .filter(|s| s.disabled_reason.is_none())
            .map(|s| mcp_server_name(&s.server))
            .collect();
        assert_eq!(
            surviving,
            ["ok"],
            "denied server must be dropped by the merge"
        );
    }

    /// A bare policy `serverName` deny drops the managed (prefixed) server as a
    /// `Denylist` hit, exact-match only (no substring over-match).
    #[test]
    fn merge_drops_server_denied_by_name_including_managed_prefix() {
        use xai_grok_workspace::permission::resolution::{AllowedMcpServer, McpServerAllowlist};

        let allowlist = McpServerAllowlist::new(
            vec![],
            vec![AllowedMcpServer::Name {
                name: "slack".into(),
            }],
            Some(std::path::PathBuf::from("/test/managed-settings.json")),
        );

        let tagged = apply_mcp_server_policy(
            vec![
                acp::McpServer::Http(
                    acp::McpServerHttp::new("grok_com_slack", "https://mcp.slack.com/sse")
                        .headers(vec![]),
                ),
                // Substring-only match must not be denied.
                acp::McpServer::Http(
                    acp::McpServerHttp::new("slackbot", "https://slackbot.example.com/mcp")
                        .headers(vec![]),
                ),
            ],
            &std::collections::HashSet::new(),
            &allowlist,
        );

        let slack = tagged
            .iter()
            .find(|s| mcp_server_name(&s.server) == "grok_com_slack")
            .expect("managed server present in policy output");
        assert!(
            matches!(
                slack.disabled_reason,
                Some(McpDisabledReason::Denylist { .. })
            ),
            "name-denied managed server must classify as Denylist, got {:?}",
            slack.disabled_reason
        );

        let bot = tagged
            .iter()
            .find(|s| mcp_server_name(&s.server) == "slackbot")
            .expect("unrelated server present in policy output");
        assert!(
            bot.disabled_reason.is_none(),
            "substring-only match must not be denied by name"
        );

        let surviving: Vec<&str> = tagged
            .iter()
            .filter(|s| s.disabled_reason.is_none())
            .map(|s| mcp_server_name(&s.server))
            .collect();
        assert_eq!(surviving, ["slackbot"]);
    }

    /// Drive the expectation through [`to_managed_name`] (not a hand-written
    /// literal) so this fails if policy/runtime name normalization ever drifts.
    #[test]
    fn policy_server_name_matches_to_managed_name_transform() {
        use xai_grok_workspace::permission::resolution::{AllowedMcpServer, McpServerAllowlist};

        let managed_server = |runtime: &str| {
            vec![acp::McpServer::Http(
                acp::McpServerHttp::new(runtime.to_string(), "https://mcp.example.com/sse")
                    .headers(vec![]),
            )]
        };
        let name_entry = |display: &str| AllowedMcpServer::Name {
            name: display.to_string(),
        };
        let source = || Some(std::path::PathBuf::from("/test/managed-settings.json"));

        for display in ["Slack", "My Server"] {
            let runtime = to_managed_name(display);

            let deny = McpServerAllowlist::new(vec![], vec![name_entry(display)], source());
            let tagged = apply_mcp_server_policy(
                managed_server(&runtime),
                &std::collections::HashSet::new(),
                &deny,
            );
            assert!(
                matches!(
                    tagged[0].disabled_reason,
                    Some(McpDisabledReason::Denylist { .. })
                ),
                "deny serverName {display:?} must block runtime {runtime:?}, got {:?}",
                tagged[0].disabled_reason
            );

            let allow = McpServerAllowlist::new(vec![name_entry(display)], vec![], source());
            let tagged = apply_mcp_server_policy(
                managed_server(&runtime),
                &std::collections::HashSet::new(),
                &allow,
            );
            assert!(
                tagged[0].disabled_reason.is_none(),
                "allow serverName {display:?} must keep runtime {runtime:?}, got {:?}",
                tagged[0].disabled_reason
            );
        }
    }

    #[test]
    fn auto_inject_dedup_by_display_name_first_scope_wins() {
        let managed = vec![
            make_managed("Linear", "https://mcp.linear.app", "user"),
            make_managed("Linear", "https://mcp.linear.app", "team"),
        ];
        let cwd = empty_cwd();
        let compat = xai_grok_tools::types::compat::CompatConfig::default();
        let merged = merge_managed_mcp_servers(vec![], cwd.path(), &managed, None, &compat);
        let linear_count = merged
            .iter()
            .filter(|s| matches!(s, acp::McpServer::Http(acp::McpServerHttp { name, .. }) if name == "grok_com_linear"))
            .count();
        assert_eq!(linear_count, 1, "should dedup by display name");
    }

    #[test]
    fn auto_inject_skips_existing_server() {
        let managed = vec![make_managed("Slack", "https://mcp.slack.com/sse", "user")];
        let client = vec![acp::McpServer::Http(
            acp::McpServerHttp::new(
                "grok_com_slack".to_string(),
                "https://mcp.slack.com/sse".to_string(),
            )
            .headers(vec![]),
        )];
        let cwd = empty_cwd();
        let compat = xai_grok_tools::types::compat::CompatConfig::default();
        let merged = merge_managed_mcp_servers(client, cwd.path(), &managed, None, &compat);
        let slack_count = merged
            .iter()
            .filter(|s| matches!(s, acp::McpServer::Http(acp::McpServerHttp { name, .. }) if name == "grok_com_slack"))
            .count();
        assert_eq!(slack_count, 1, "should not duplicate existing server");
    }

    #[test]
    fn auto_inject_skips_disabled() {
        let managed = vec![
            make_managed("Slack", "https://mcp.slack.com/sse", "user"),
            make_managed("Linear", "https://mcp.linear.app", "user"),
        ];
        let disabled: std::collections::HashSet<String> =
            ["grok_com_slack".to_string()].into_iter().collect();
        let mut merged = vec![];
        auto_inject_managed_servers_with_disabled(&mut merged, &managed, &disabled);

        let has_slack = merged
            .iter()
            .any(|s| matches!(s, acp::McpServer::Http(acp::McpServerHttp { name, .. }) if name == "grok_com_slack"));
        let has_linear = merged
            .iter()
            .any(|s| matches!(s, acp::McpServer::Http(acp::McpServerHttp { name, .. }) if name == "grok_com_linear"));
        assert!(!has_slack, "disabled connector should be skipped");
        assert!(has_linear, "non-disabled connector should be injected");
    }

    #[test]
    fn lower_precedence_http_servers_are_blocked_by_toml_name_claims() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(cwd.path().join(".grok")).unwrap();
        std::fs::write(
            cwd.path().join(".grok").join("config.toml"),
            r#"
[mcp_servers.github]
url = "https://config.example.com/mcp"
enabled = false
"#,
        )
        .unwrap();
        git2::Repository::init(cwd.path()).unwrap();
        std::fs::write(
            cwd.path().join(".mcp.json"),
            r#"{
                "mcpServers": {
                    "github": {
                        "url": "https://json.example.com/mcp"
                    }
                }
            }"#,
        )
        .unwrap();

        let compat = xai_grok_tools::types::compat::CompatConfig::default();
        let merged = merge_managed_mcp_servers(vec![], cwd.path(), &[], None, &compat);
        assert!(
            !merged.iter().any(|server| matches!(
                server,
                acp::McpServer::Http(acp::McpServerHttp { name, .. }) if name == "github"
            )),
            "config.toml should block same-named lower-precedence HTTP servers"
        );
    }

    /// End-to-end folder-trust gate through the public merge: an untrusted
    /// workspace's project `.mcp.json` server is dropped before spawn (a
    /// client-supplied server still survives), while a trusted workspace keeps
    /// it. Existing merge tests record no decision, so the default-allowed gate
    /// leaves them a no-op.
    #[test]
    fn untrusted_workspace_drops_project_mcp_servers() {
        fn repo_with_project_server() -> tempfile::TempDir {
            let cwd = tempfile::tempdir().unwrap();
            git2::Repository::init(cwd.path()).unwrap();
            std::fs::write(
                cwd.path().join(".mcp.json"),
                r#"{"mcpServers": {"projsrv": {"url": "https://proj.example.com/mcp"}}}"#,
            )
            .unwrap();
            cwd
        }
        let compat = xai_grok_tools::types::compat::CompatConfig::default();

        let untrusted = repo_with_project_server();
        crate::agent::folder_trust::record_for_test(untrusted.path(), false);
        let client = vec![acp::McpServer::Http(
            acp::McpServerHttp::new(
                "clientsrv".to_string(),
                "https://client.example.com/mcp".to_string(),
            )
            .headers(vec![]),
        )];
        let merged = merge_managed_mcp_servers(client, untrusted.path(), &[], None, &compat);
        assert!(
            !merged.iter().any(|s| mcp_server_name(s) == "projsrv"),
            "untrusted workspace must drop its repo-local MCP server"
        );
        assert!(
            merged.iter().any(|s| mcp_server_name(s) == "clientsrv"),
            "client-supplied server must be retained when untrusted"
        );

        let trusted = repo_with_project_server();
        crate::agent::folder_trust::record_for_test(trusted.path(), true);
        let merged = merge_managed_mcp_servers(vec![], trusted.path(), &[], None, &compat);
        assert!(
            merged.iter().any(|s| mcp_server_name(s) == "projsrv"),
            "trusted workspace must keep its repo-local MCP server"
        );
    }

    #[test]
    fn load_plugin_mcp_creates_stdio_server_with_env_substitution() {
        let config: crate::util::config::McpConfig = serde_json::from_value(serde_json::json!({
            "mcpServers": {
                "echo-mcp": {
                    "command": "python3",
                    "args": ["${GROK_PLUGIN_ROOT}/mcp-echo-server.py"]
                }
            }
        }))
        .expect("parse test MCP config");

        let (servers, _) = load_plugin_mcp_servers_from_config(
            &config,
            "team-tool",
            "/home/user/.grok/plugins/team-tool",
            "/home/user/.grok/plugin-data/team-tool",
        );

        assert_eq!(servers.len(), 1, "should create one server");
        match &servers[0] {
            acp::McpServer::Stdio(acp::McpServerStdio {
                name,
                command,
                args,
                ..
            }) => {
                assert_eq!(name, "echo-mcp");
                assert_eq!(command.display().to_string(), "python3");
                assert_eq!(
                    args.as_slice(),
                    &["/home/user/.grok/plugins/team-tool/mcp-echo-server.py"]
                );
            }
            other => panic!("expected Stdio server, got {:?}", other),
        }
    }

    #[test]
    fn load_plugin_mcp_disabled_server_excluded_from_merge() {
        let config: crate::util::config::McpConfig = serde_json::from_value(serde_json::json!({
            "mcpServers": {
                "test-server": {
                    "command": "node",
                    "args": ["server.js"]
                }
            }
        }))
        .expect("parse test MCP config");

        let (servers, _) = load_plugin_mcp_servers_from_config(
            &config,
            "my-plugin",
            "/tmp/plugin",
            "/tmp/plugin-data",
        );
        assert_eq!(servers.len(), 1);

        // Simulate disabling via disabled_mcp_server_names.
        let disabled: std::collections::HashSet<String> =
            ["test-server".to_string()].into_iter().collect();

        // auto_inject_managed_servers_with_disabled is for managed servers;
        // for plugin servers, the disabled check happens during merge.
        // Verify the server name matches what would be checked.
        assert!(
            disabled.contains("test-server"),
            "disabled set should contain the server name used in .mcp.json"
        );
    }

    #[test]
    fn load_plugin_mcp_from_value_accepts_direct_map() {
        let value = serde_json::json!({
            "sentry": { "type": "http", "url": "https://mcp.sentry.dev/mcp" }
        });
        let (servers, _) =
            load_plugin_mcp_servers_from_value(&value, "sentry", "/tmp/p", "/tmp/pd");
        assert_eq!(servers.len(), 1);
        match &servers[0] {
            acp::McpServer::Http(acp::McpServerHttp { name, url, .. }) => {
                assert_eq!(name, "sentry");
                assert_eq!(url, "https://mcp.sentry.dev/mcp");
            }
            other => panic!("expected Http server, got {:?}", other),
        }
    }

    #[test]
    fn plugin_server_deduped_across_file_and_inline() {
        use xai_grok_agent::plugins::PluginRegistry;
        use xai_grok_agent::plugins::PluginScope;
        use xai_grok_agent::plugins::discovery::{DiscoveredPlugin, PluginId};
        use xai_grok_agent::plugins::manifest::{PathOrInline, PluginManifest};

        let tmp = tempfile::tempdir().unwrap();
        let plugin_root = tmp.path().join("sentry");
        std::fs::create_dir_all(&plugin_root).unwrap();
        let mcp_json = plugin_root.join(".mcp.json");
        std::fs::write(
            &mcp_json,
            r#"{"mcpServers":{"sentry":{"type":"http","url":"https://mcp.sentry.dev/mcp"}}}"#,
        )
        .unwrap();

        let manifest = PluginManifest {
            name: "sentry".into(),
            version: None,
            description: None,
            author: None,
            homepage: None,
            repository: None,
            license: None,
            keywords: vec![],
            skills: None,
            commands: None,
            agents: None,
            hooks: None,
            mcp_servers: Some(PathOrInline::Inline(serde_json::json!({
                "sentry": { "type": "http", "url": "https://mcp.sentry.dev/mcp" }
            }))),
            lsp_servers: None,
        };
        let id = PluginId::new(PluginScope::User, &plugin_root, "sentry");
        let dp = DiscoveredPlugin {
            manifest,
            id,
            root: plugin_root.clone(),
            canonical_root: plugin_root.clone(),
            scope: PluginScope::User,
            origin: xai_grok_agent::plugins::PluginOrigin::UserGrok,
            trusted: true,
            skill_dirs: vec![],
            command_dirs: vec![],
            agent_dirs: vec![],
            hooks_path: None,
            mcp_config_path: Some(mcp_json),
            lsp_config_path: None,
            conflict: None,
        };
        let registry = PluginRegistry::from_discovered(vec![dp], &[], &["sentry".to_string()]);

        let cwd = tempfile::tempdir().unwrap();
        let compat = xai_grok_tools::types::compat::CompatConfig::default();
        let sourced = merge_managed_mcp_servers_sourced(cwd.path(), Some(&registry), &compat);

        let sentry_count = sourced
            .iter()
            .filter(|(s, _)| mcp_server_name(s) == "sentry")
            .count();
        assert_eq!(
            sentry_count, 1,
            "sentry declared in both .mcp.json and inline must register exactly once"
        );
    }

    #[test]
    fn plugin_same_name_different_url_keeps_file_server() {
        use xai_grok_agent::plugins::PluginRegistry;
        use xai_grok_agent::plugins::PluginScope;
        use xai_grok_agent::plugins::discovery::{DiscoveredPlugin, PluginId};
        use xai_grok_agent::plugins::manifest::{PathOrInline, PluginManifest};

        let tmp = tempfile::tempdir().unwrap();
        let plugin_root = tmp.path().join("sentry");
        std::fs::create_dir_all(&plugin_root).unwrap();
        let mcp_json = plugin_root.join(".mcp.json");
        std::fs::write(
            &mcp_json,
            r#"{"mcpServers":{"sentry":{"type":"http","url":"https://file.example/mcp"}}}"#,
        )
        .unwrap();

        let manifest = PluginManifest {
            name: "sentry".into(),
            version: None,
            description: None,
            author: None,
            homepage: None,
            repository: None,
            license: None,
            keywords: vec![],
            skills: None,
            commands: None,
            agents: None,
            hooks: None,
            mcp_servers: Some(PathOrInline::Inline(serde_json::json!({
                "sentry": { "type": "http", "url": "https://inline.example/mcp" }
            }))),
            lsp_servers: None,
        };
        let id = PluginId::new(PluginScope::User, &plugin_root, "sentry");
        let dp = DiscoveredPlugin {
            manifest,
            id,
            root: plugin_root.clone(),
            canonical_root: plugin_root.clone(),
            scope: PluginScope::User,
            origin: xai_grok_agent::plugins::PluginOrigin::UserGrok,
            trusted: true,
            skill_dirs: vec![],
            command_dirs: vec![],
            agent_dirs: vec![],
            hooks_path: None,
            mcp_config_path: Some(mcp_json),
            lsp_config_path: None,
            conflict: None,
        };
        let registry = PluginRegistry::from_discovered(vec![dp], &[], &["sentry".to_string()]);

        let cwd = tempfile::tempdir().unwrap();
        let compat = xai_grok_tools::types::compat::CompatConfig::default();
        let sourced = merge_managed_mcp_servers_sourced(cwd.path(), Some(&registry), &compat);

        let sentry: Vec<&acp::McpServer> = sourced
            .iter()
            .map(|(s, _)| s)
            .filter(|s| mcp_server_name(s) == "sentry")
            .collect();
        assert_eq!(
            sentry.len(),
            1,
            "same plugin declaring one server name twice must register exactly once"
        );
        match sentry[0] {
            acp::McpServer::Http(acp::McpServerHttp { url, .. }) => {
                assert_eq!(url, "https://file.example/mcp", "file source must win");
            }
            other => panic!("expected Http server, got {:?}", other),
        }
    }

    #[test]
    fn collect_plugin_oauth_configs_reads_byo_client_id_from_mcp_json() {
        use xai_grok_agent::plugins::PluginRegistry;
        use xai_grok_agent::plugins::PluginScope;
        use xai_grok_agent::plugins::discovery::{DiscoveredPlugin, PluginId};
        use xai_grok_agent::plugins::manifest::PluginManifest;

        let tmp = tempfile::tempdir().unwrap();
        let plugin_root = tmp.path().join("slack");
        std::fs::create_dir_all(&plugin_root).unwrap();
        let mcp_json = plugin_root.join(".mcp.json");
        std::fs::write(
            &mcp_json,
            r#"{"mcpServers":{"slack":{"type":"http","url":"https://mcp.slack.example/mcp","oauth":{"clientId":"slack-byo-client","callbackPort":3118}}}}"#,
        )
        .unwrap();

        let manifest = PluginManifest {
            name: "slack".into(),
            version: None,
            description: None,
            author: None,
            homepage: None,
            repository: None,
            license: None,
            keywords: vec![],
            skills: None,
            commands: None,
            agents: None,
            hooks: None,
            mcp_servers: None,
            lsp_servers: None,
        };
        let id = PluginId::new(PluginScope::User, &plugin_root, "slack");
        let dp = DiscoveredPlugin {
            manifest,
            id,
            root: plugin_root.clone(),
            canonical_root: plugin_root.clone(),
            scope: PluginScope::User,
            origin: xai_grok_agent::plugins::PluginOrigin::UserGrok,
            trusted: true,
            skill_dirs: vec![],
            command_dirs: vec![],
            agent_dirs: vec![],
            hooks_path: None,
            mcp_config_path: Some(mcp_json),
            lsp_config_path: None,
            conflict: None,
        };
        let registry = PluginRegistry::from_discovered(vec![dp], &[], &["slack".to_string()]);

        let oauth = collect_plugin_oauth_configs(Some(&registry));
        assert_eq!(
            oauth
                .get("slack")
                .expect("slack oauth")
                .client_id
                .as_deref(),
            Some("slack-byo-client")
        );
    }

    #[test]
    fn collect_plugin_oauth_configs_none_registry_is_empty() {
        assert!(collect_plugin_oauth_configs(None).is_empty());
    }

    #[test]
    fn merge_plugin_oauth_respects_source_precedence() {
        use crate::util::config::{McpOAuthConfig, McpOAuthConfigMap};

        let byo = |id: &str| McpOAuthConfig {
            client_id: Some(id.to_string()),
            ..Default::default()
        };

        let mut base = McpOAuthConfigMap::new();
        base.insert("shared".to_string(), byo("file-client"));
        base.insert("toml-svc".to_string(), byo("toml-client"));

        let mut plugin = McpOAuthConfigMap::new();
        plugin.insert("shared".to_string(), byo("plugin-client"));
        plugin.insert("toml-svc".to_string(), byo("plugin-shadow"));
        plugin.insert("plugin-only".to_string(), byo("plugin-only-client"));

        let toml_names: std::collections::HashSet<String> =
            ["toml-svc".to_string()].into_iter().collect();
        merge_plugin_oauth_into(&mut base, plugin, &toml_names);

        assert_eq!(
            base.get("shared").unwrap().client_id.as_deref(),
            Some("plugin-client")
        );
        assert_eq!(
            base.get("toml-svc").unwrap().client_id.as_deref(),
            Some("toml-client")
        );
        assert_eq!(
            base.get("plugin-only").unwrap().client_id.as_deref(),
            Some("plugin-only-client")
        );
    }
}
