//! `grok inspect` — configuration introspection.
//!
//! Shows everything Grok discovers in the current directory: project
//! instructions, permissions, hooks, skills, agents, plugins, MCP servers,
//! LSP config, and config.toml sources. Supports `--json` for machine output.

mod compat;

pub use compat::{CompatEntryStatus, CompatSource, ExternalCompatEntry, ExternalCompatReport};
use compat::{
    derive_vendor, instruction_compat_status, resolve_inspect_compat, vendor_compat_status,
    vendor_tag,
};

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::auth::ForceLoginTeam;
use xai_grok_tools::types::config_source::ConfigSource;
use xai_grok_tools::util::truncate::estimate_tokens;

const TREE: &str = "\u{2514}";

/// Coarse scope label for project instructions and plugin entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Project,
    User,
    Global,
    Plugin,
    Builtin,
    Cli,
    Config,
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Project => "project",
            Self::User => "user",
            Self::Global => "global",
            Self::Plugin => "plugin",
            Self::Builtin => "builtin",
            Self::Cli => "cli",
            Self::Config => "config",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectReport {
    pub grok_version: String,
    pub channel: String,
    pub cwd: String,
    pub project_root: Option<String>,
    /// Folder-trust verdict for `cwd`: when false, repo-local project hooks,
    /// plugins, and MCP/LSP entries are gated out of the listings below.
    pub project_trusted: bool,
    pub project_instructions: Vec<InstructionFile>,
    pub permissions: PermissionsReport,
    pub login_policy: LoginPolicyReport,
    pub hooks: Vec<HookEntry>,
    pub skills: Vec<SkillEntry>,
    pub agents: Vec<AgentEntry>,
    pub plugins: Vec<PluginEntry>,
    pub marketplaces: Vec<MarketplaceEntry>,
    pub mcp_servers: Vec<McpServerEntry>,
    pub lsp_servers: Vec<LspServerEntry>,
    pub config_sources: ConfigSources,
    pub external_compat: ExternalCompatReport,
    /// Warnings from `[model.*]` and `[auth_provider.*]` parsing.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub config_warnings: Vec<crate::agent::config_model_override_parse::ConfigWarning>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstructionFile {
    pub path: String,
    pub scope: Scope,
    pub file_type: String,
    pub size_bytes: usize,
    /// Estimated token count (chars / 4).
    pub approx_tokens: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// True when this entry's vendor surface is disabled by compat config.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility_status: Option<CompatEntryStatus>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsReport {
    pub sources: Vec<String>,
    pub loaded: usize,
    pub skipped: Vec<SkippedRule>,
    pub mcp_server_allowlist: Vec<String>,
    pub marketplace_allowlist: Vec<String>,
    /// Platform path for managed-settings.json vendor policy (None on unsupported OS).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed_settings_path: Option<String>,
    /// Whether that file exists on disk. Always emitted, so a JSON consumer
    /// can distinguish "absent" from "present" without string-matching.
    pub managed_settings_exists: bool,
    /// Whether the runtime actually loaded that file into policy (`exists` can
    /// be true while this is false for an unreadable/malformed file). Always emitted.
    pub managed_settings_active: bool,
    /// Settings forced by a policy layer.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub enforced: Vec<EnforcedPolicy>,
}

/// One policy-enforced setting. Structured for `--json`; the human view
/// derives its line from these fields (see `enforced_label`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnforcedPolicy {
    /// Stable key: "alwaysApprove" | "telemetry" | "feedback".
    pub setting: String,
    /// The enforced value.
    pub enabled: bool,
    /// Originating file, e.g. "managed-settings.json".
    pub source: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedRule {
    pub rule: String,
    pub reason: String,
}

/// Enterprise login-hardening policy resolved from `[grok_com_config]`
/// (TOML + env). Surfaced so admins can verify the deployment loaded it.
/// The team pin is admin policy, not a secret, so it is shown verbatim.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginPolicyReport {
    /// Raw `disable_api_key_auth` knob (env `GROK_DISABLE_API_KEY_AUTH`).
    pub disable_api_key_auth: Option<bool>,
    /// Configured team pin: single string, list, or null when unset.
    pub force_login_team_uuid: Option<ForceLoginTeam>,
    /// Resolved verdict — true when either knob forces first-party login.
    pub api_key_auth_disabled: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookEntry {
    pub event: String,
    pub hook_type: String,
    pub target: String,
    pub source: ConfigSource,
    pub matcher: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// True when this entry's vendor surface is disabled by compat config.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility_status: Option<CompatEntryStatus>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    pub source: ConfigSource,
    pub user_invocable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// True when disabled by `[skills].disabled` config or when this entry's
    /// vendor surface is disabled by compat config.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility_status: Option<CompatEntryStatus>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEntry {
    pub name: String,
    pub description: String,
    pub source: ConfigSource,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginEntry {
    pub name: String,
    pub scope: Scope,
    pub path: String,
    pub enabled: bool,
    pub provides: PluginProvides,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginProvides {
    pub skills: usize,
    pub agents: usize,
    pub hooks: bool,
    pub mcp_servers: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceEntry {
    pub name: String,
    pub path: String,
    pub enabled_plugins: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerEntry {
    pub name: String,
    pub transport: String,
    pub target: String,
    pub source: ConfigSource,
    /// True when this entry's vendor surface is disabled by compat config.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility_status: Option<CompatEntryStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspServerEntry {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub source: ConfigSource,
    pub extensions: Vec<String>,
    /// True when this project-scoped server would be skipped (untrusted folder).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub untrusted: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSources {
    /// Config layers (system + user managed, user + system requirements, user
    /// config.toml, the macOS MDM managed-preferences layer, and project
    /// .grok/config.toml files). Driven from the same resolvers used at runtime
    /// (`ConfigLayers`, `requirements_layers`) so system + MDM layers and
    /// precedence are included, and emptiness reflects real contribution after
    /// stripping (version_overrides, fail_closed, etc).
    pub layers: Vec<ConfigLayer>,
}

/// A single config layer entry for `grok inspect`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigLayer {
    /// Logical role of the layer: "system-managed", "managed", "user",
    /// "system-requirements", "requirements", "mdm", or "project".
    pub role: String,
    pub path: String,
    /// "empty" or "parse error" when the on-disk file does not contribute
    /// effective config (after the real loader's processing). Omitted when
    /// the layer is present and contributes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

pub async fn inspect(cwd: &Path, json: bool) -> anyhow::Result<()> {
    let report = build_report(cwd).await;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_human(&report);
    }

    Ok(())
}

async fn build_report(cwd: &Path) -> InspectReport {
    let effective_config_result = crate::config::load_effective_config();
    let effective_config = effective_config_result
        .as_ref()
        .cloned()
        .unwrap_or_else(|_| toml::Value::Table(toml::map::Map::new()));
    // Parse compatibility separately so malformed cells cannot block unrelated sections.
    let mut config_without_compat = effective_config.clone();
    if let Some(table) = config_without_compat.as_table_mut() {
        table.remove("compat");
    }
    let parsed_config =
        crate::agent::config::Config::new_from_toml_cfg(&config_without_compat).ok();

    let git_root = git2::Repository::discover(cwd)
        .ok()
        .and_then(|r| r.workdir().map(|p| p.to_path_buf()));

    // Route through the live folder-trust gate rather than a raw store read; no
    // session resolve has run for a one-shot `inspect`. The single verdict drives
    // the top-level flag and gates the hooks, plugins, and MCP/LSP listings so
    // they reflect runtime gating. `remote = None`: env/user/managed opt-out is
    // honored, but a remote kill-switch is not consulted on this report-only path.
    crate::agent::folder_trust::resolve_and_record(cwd, None, false);
    let project_trusted = crate::agent::folder_trust::project_scope_allowed(cwd);

    let trust_store = xai_grok_agent::plugins::TrustStore::load();
    let mut plugins_cfg: crate::agent::config::PluginsConfig = effective_config
        .get("plugins")
        .and_then(|v| v.clone().try_into().ok())
        .unwrap_or_default();
    plugins_cfg.merge_claude_enabled_plugins(Some(cwd));
    let mut plugin_config = plugins_cfg.to_discovery_config();
    // Project plugins gate on the same folder-trust verdict as hooks and the live
    // session/doctor sites, so the listing's `enabled` flags match runtime gating.
    let discovered_plugins = xai_grok_agent::plugins::discover_plugins(
        Some(cwd),
        &plugin_config,
        &trust_store,
        project_trusted,
    );
    plugin_config.populate_plugin_lists(&discovered_plugins);

    let plugin_registry = xai_grok_agent::plugins::PluginRegistry::from_discovered(
        discovered_plugins.clone(),
        &plugin_config.disabled,
        &plugin_config.enabled,
    );

    let external_compat = resolve_inspect_compat(effective_config_result.as_ref().map_err(|_| ()));

    // Same `[skills]` table the runtime loads, so `paths` skills appear,
    // `ignore`d ones are hidden, and `disabled` ones surface as disabled.
    let skills_config = crate::config::parse_skills_config(&effective_config);

    // Discover with all vendors ON so inspect shows the full set on disk.
    let (mut instructions, permissions, mut skills) = tokio::join!(
        list_instructions(cwd),
        list_permissions(cwd),
        list_skills(cwd, &plugin_registry, &skills_config),
    );

    // Attach local compatibility status to each discovered vendor entry.
    for entry in &mut instructions {
        entry.compatibility_status =
            instruction_compat_status(&entry.vendor, &entry.file_type, &external_compat);
        entry.disabled |= entry.compatibility_status == Some(CompatEntryStatus::Disabled);
    }
    for entry in &mut skills {
        entry.compatibility_status =
            vendor_compat_status(&entry.vendor, "skills", &external_compat);
        entry.disabled |= entry.compatibility_status == Some(CompatEntryStatus::Disabled);
    }
    let mut hooks = list_hooks(git_root.as_deref(), project_trusted, &discovered_plugins);
    for entry in &mut hooks {
        entry.compatibility_status = vendor_compat_status(&entry.vendor, "hooks", &external_compat);
        entry.disabled |= entry.compatibility_status == Some(CompatEntryStatus::Disabled);
    }
    let agents = list_agents(cwd, &plugin_registry);
    let plugins = list_plugins(&discovered_plugins);
    let marketplaces = list_marketplaces(git_root.as_deref());
    let mut mcp = list_mcp_servers(cwd, &plugin_registry);
    for entry in &mut mcp {
        entry.compatibility_status = vendor_compat_status(&entry.vendor, "mcps", &external_compat);
        entry.disabled |= entry.compatibility_status == Some(CompatEntryStatus::Disabled);
    }
    let lsp = list_lsp_servers(cwd, &discovered_plugins);
    let configs = list_config_sources(cwd);
    let config_warnings = parsed_config
        .as_ref()
        .map(|c| c.config_warnings.clone())
        .unwrap_or_default();

    InspectReport {
        grok_version: xai_grok_version::VERSION.to_string(),
        channel: crate::util::config::channel_name_from_cache()
            .unwrap_or("unknown")
            .to_string(),
        cwd: cwd.display().to_string(),
        project_root: git_root.map(|p| p.display().to_string()),
        project_trusted,
        project_instructions: instructions,
        permissions,
        login_policy: login_policy_report(parsed_config.as_ref()),
        hooks,
        skills,
        agents,
        plugins,
        marketplaces,
        mcp_servers: mcp,
        lsp_servers: lsp,
        config_sources: configs,
        external_compat,
        config_warnings,
    }
}

/// Read `[paths] extra_rule_dirs` from the effective config. Returns empty
/// on any read/parse failure so misconfiguration never breaks classification.
fn extra_rule_dirs_from_config() -> Vec<String> {
    let Ok(root) = crate::config::load_effective_config() else {
        return Vec::new();
    };
    root.get("paths")
        .and_then(|v| v.get("extra_rule_dirs"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn has_rules_directory(file_path: &str, config_dir: &str) -> bool {
    let mut previous = None;
    for component in file_path
        .split(['/', '\\'])
        .filter(|component| !component.is_empty())
    {
        if previous == Some(config_dir) && component == "rules" {
            return true;
        }
        previous = Some(component);
    }
    false
}

fn instruction_scope(
    file_path: &str,
    grok_home: &Path,
    vendor_homes: &[(PathBuf, bool)],
    workspace_root: &Path,
) -> Scope {
    if crate::util::is_user_instruction_path(
        Path::new(file_path),
        grok_home,
        vendor_homes,
        Some(workspace_root),
    ) {
        Scope::Global
    } else {
        Scope::Project
    }
}

fn instruction_file_type(
    file_path: &str,
    grok_home: &Path,
    claude_imported: bool,
    extra_rule_prefixes: &[PathBuf],
) -> &'static str {
    let path = Path::new(file_path);
    if path
        .parent()
        .is_some_and(|parent| parent == grok_home.join("rules"))
        || has_rules_directory(file_path, ".grok")
        || has_rules_directory(file_path, ".cursor")
        || (!claude_imported && has_rules_directory(file_path, ".claude"))
        || extra_rule_prefixes
            .iter()
            .any(|prefix| path.starts_with(prefix))
    {
        "rules"
    } else {
        "agents_md"
    }
}

/// Wraps the production instruction discovery (`agents_md::read_agents_config_with_paths`).
async fn list_instructions(cwd: &Path) -> Vec<InstructionFile> {
    // Discover with all vendors ON so inspect shows the full set.
    let configs = xai_grok_agent::prompt::agents_md::read_agents_config_with_paths(
        &cwd.display().to_string(),
        xai_grok_agent::prompt::skills::CompatConfig::default(),
    )
    .await;

    let grok_home = crate::util::grok_home::grok_home();
    let vendor_homes = dirs::home_dir()
        .map(|home_dir| {
            vec![
                (home_dir.join(".claude"), true),
                (home_dir.join(".cursor"), true),
            ]
        })
        .unwrap_or_default();
    let workspace_root = git2::Repository::discover(cwd)
        .ok()
        .and_then(|repo| repo.workdir().map(Path::to_path_buf))
        .unwrap_or_else(|| cwd.to_path_buf());

    // Phase 2 cutoff: when imported, stop classifying `.claude/rules/` paths
    // as rules. Equivalent dirs come in via `[paths] extra_rule_dirs`.
    let imported = crate::claude_import::is_claude_import_marked();
    let extra_rule_dirs = extra_rule_dirs_from_config();
    // Pre-expand `~/` and resolve once, so the per-config-file matching loop
    // can use a clean prefix check. Empty/invalid paths fall
    // through to a no-op match.
    //
    // TODO(phase-3): `extra_rule_dirs` only re-classifies files that
    // `xai_grok_agent::prompt::agents_md::read_agents_config_with_paths`
    // has already discovered. Plumbing `extra_rule_dirs` through to that
    // discovery (so files in arbitrary user-configured dirs are surfaced as
    // rules instead of being missed entirely) is out of scope for this stack
    // (intentional wontfix for now).
    // Skills (`extensions/skills.rs`) take the typed-scan path so they don't
    // have this limitation; rules need the same treatment in a follow-up.
    let extra_rule_prefixes: Vec<std::path::PathBuf> = extra_rule_dirs
        .iter()
        .map(|d| crate::claude_import::expand_home(d))
        .collect();

    configs
        .into_iter()
        .map(|c| {
            let file_type =
                instruction_file_type(&c.file_path, &grok_home, imported, &extra_rule_prefixes);
            let scope = instruction_scope(&c.file_path, &grok_home, &vendor_homes, &workspace_root);
            let size = c.content.len();
            let vendor = derive_vendor(&c.file_path).map(String::from);
            InstructionFile {
                size_bytes: size,
                approx_tokens: estimate_tokens(&c.content),
                path: c.file_path,
                scope,
                file_type: file_type.to_string(),
                vendor,
                disabled: false,
                compatibility_status: None,
            }
        })
        .collect()
}

/// Calls the production permission resolver (`resolve_permissions_with_provenance`)
/// which handles both Grok TOML and vendor settings fallback in one codepath.
async fn list_permissions(cwd: &Path) -> PermissionsReport {
    use xai_grok_workspace::permission::resolution;

    let ms = resolution::managed_settings();
    let format_entry = |e: &resolution::AllowedMcpServer| match e {
        resolution::AllowedMcpServer::Http { url_pattern } => url_pattern.clone(),
        resolution::AllowedMcpServer::Stdio { command } => format!("command:{command}"),
        resolution::AllowedMcpServer::Name { name } => format!("name:{name}"),
    };
    let mcp_server_allowlist: Vec<String> = ms
        .mcp_allowlist
        .entries
        .iter()
        .map(format_entry)
        .chain(
            ms.mcp_allowlist
                .deny_entries
                .iter()
                .map(|e| format!("deny:{}", format_entry(e))),
        )
        .collect();
    let marketplace_allowlist = ms.marketplace_allowlist.allowed_urls.clone();

    // Managed settings presence + enforced policy computed unconditionally (before
    // the early return) so that a managed-settings.json containing *only* e.g.
    // disableBypassPermissionsMode still surfaces its path and effects.
    let managed_settings_path =
        crate::config::claude_managed_settings_probe_path().map(|p| p.display().to_string());
    let managed_settings_exists =
        crate::config::claude_managed_settings_probe_path().is_some_and(|p| p.exists());
    // `source_path` is set only on the successful read+parse path, so it is the
    // signal for "actually loaded" (vs present-but-broken).
    let managed_settings_active = ms.features.source_path.is_some();

    let mut enforced = Vec::new();
    if let Some(src) = &ms.features.source_path {
        let source = src
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "managed-settings.json".to_string());
        for (flag, setting) in [
            (ms.features.disable_yolo, "alwaysApprove"),
            (ms.features.disable_telemetry, "telemetry"),
            (ms.features.disable_feedback, "feedback"),
        ] {
            if flag == Some(true) {
                enforced.push(EnforcedPolicy {
                    setting: setting.to_string(),
                    enabled: false,
                    source: source.clone(),
                });
            }
        }
    }

    let Some(resolved) = resolution::resolve_permissions_with_provenance(cwd).await else {
        return PermissionsReport {
            sources: vec![],
            loaded: 0,
            skipped: vec![],
            mcp_server_allowlist,
            marketplace_allowlist,
            managed_settings_path: managed_settings_path.clone(),
            managed_settings_exists,
            managed_settings_active,
            enforced: enforced.clone(),
        };
    };

    let mut sources: Vec<String> = resolved.sources.iter().map(|s| s.to_string()).collect();
    sources.dedup();

    let skipped = resolved
        .skipped
        .into_iter()
        .map(|s| SkippedRule {
            rule: s.rule,
            reason: s.reason,
        })
        .collect();

    PermissionsReport {
        sources,
        loaded: resolved.config.rules.len(),
        skipped,
        mcp_server_allowlist,
        marketplace_allowlist,
        managed_settings_path,
        managed_settings_exists,
        managed_settings_active,
        enforced,
    }
}

/// Resolves the enterprise login-hardening knobs from the merged config
/// (`[grok_com_config]`, the `[auth]` alias, and env overrides) so admins can
/// confirm the deployment's auth policy actually loaded.
fn login_policy_report(config: Option<&crate::agent::config::Config>) -> LoginPolicyReport {
    let grok_com_config = config
        .map(|c| c.grok_com_config.clone())
        .unwrap_or_default();
    LoginPolicyReport {
        api_key_auth_disabled: grok_com_config.api_key_auth_disabled(),
        disable_api_key_auth: grok_com_config.disable_api_key_auth,
        force_login_team_uuid: grok_com_config.force_login_team_uuid,
    }
}

/// Discovers hooks with every vendor enabled so compatibility can be annotated later.
fn list_hooks(
    git_root: Option<&Path>,
    project_trusted: bool,
    discovered_plugins: &[xai_grok_agent::plugins::DiscoveredPlugin],
) -> Vec<HookEntry> {
    let all_on = xai_grok_tools::types::compat::CompatConfig::default();
    let source_paths = crate::util::hooks::discover_hook_source_paths(git_root, &all_on);
    let (global_sources, project_sources) = source_paths.as_sources(project_trusted);

    let (registry, _errors) =
        xai_grok_hooks::discovery::load_hooks_from_sources(&global_sources, &project_sources);

    let home_dir = dirs::home_dir();
    let grok_home = xai_grok_config::grok_home();

    let mut entries: Vec<HookEntry> = registry
        .all_hooks()
        .into_iter()
        .map(|h| {
            let is_user_scope = h.source_dir.starts_with(&grok_home)
                || home_dir.as_deref().is_some_and(|home| {
                    h.source_dir.starts_with(home.join(".cursor"))
                        || h.source_dir.starts_with(home.join(".claude"))
                });
            let source = if is_user_scope {
                ConfigSource::User {
                    path: h.source_dir.clone(),
                }
            } else {
                ConfigSource::Project {
                    path: h.source_dir.clone(),
                }
            };
            let vendor = derive_vendor(&h.source_dir.display().to_string()).map(String::from);
            HookEntry {
                event: format!("{:?}", h.event),
                hook_type: h.handler_type.as_str().to_string(),
                target: h
                    .command
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .or_else(|| h.url.clone())
                    .unwrap_or_default(),
                source,
                matcher: h.configured_matcher.clone(),
                vendor,
                disabled: false,
                compatibility_status: None,
            }
        })
        .collect();

    // Plugin hooks
    for p in discovered_plugins {
        if !p.trusted {
            continue;
        }
        let source = ConfigSource::Plugin {
            plugin_name: p.manifest.name.clone(),
            path: p.root.clone(),
        };
        if let Some(ref hooks_path) = p.hooks_path {
            entries.push(HookEntry {
                event: "(plugin)".to_string(),
                hook_type: "file".to_string(),
                target: hooks_path.display().to_string(),
                source,
                matcher: None,
                vendor: None,
                disabled: false,
                compatibility_status: None,
            });
        } else if p.manifest.inline_hooks().is_some() {
            entries.push(HookEntry {
                event: "(plugin)".to_string(),
                hook_type: "inline".to_string(),
                target: String::new(),
                source,
                matcher: None,
                vendor: None,
                disabled: false,
                compatibility_status: None,
            });
        }
    }

    entries
}

async fn list_skills(
    cwd: &Path,
    plugin_registry: &xai_grok_agent::plugins::PluginRegistry,
    skills_config: &xai_grok_agent::prompt::skills::SkillsConfig,
) -> Vec<SkillEntry> {
    // Discover with all vendors ON so inspect shows the full set.
    let skills = xai_grok_agent::prompt::skills::list_skills_with_plugins(
        Some(&cwd.display().to_string()),
        skills_config,
        Some(plugin_registry),
        xai_grok_agent::prompt::skills::CompatConfig::default(),
    )
    .await;

    let grok_home = crate::util::grok_home::grok_home();
    skills
        .into_iter()
        .map(|s| {
            let source = skill_entry_source(&s, &grok_home);
            let vendor = derive_vendor(&s.path).map(String::from);
            SkillEntry {
                name: s.label().to_string(),
                description: s.description,
                source,
                user_invocable: s.user_invocable,
                vendor,
                // Preserve `[skills].disabled`; compatibility is applied later.
                disabled: !s.enabled,
                compatibility_status: None,
            }
        })
        .collect()
}

/// Resolve the inspect-facing source for a discovered skill.
///
/// Prefers the discovery-stamped `config_source` (plugin skills,
/// `[skills].paths` entries), then falls back to a scope mapping. One
/// display-only fixup: bundled skills are extracted to
/// `<grok_home>/skills/<name>/SKILL.md` and discovered as user skills, so a
/// skill at exactly that path with a bundled name is re-labeled `Bundled`
/// (`builtin::is_extracted_bundled_skill`) — a same-named skill anywhere else
/// stays non-bundled. Runtime discovery scopes/precedence are untouched.
///
/// `Bundled`/`Server` sources are constructed only here, never by runtime
/// discovery: deployed pagers parse `x.ai/skills/list` into a typed
/// `ConfigSource` and reject unknown tags, so runtime stamping must wait
/// until clients without these variants have aged out. Until then this
/// mapping is the single owner of the scope→source translation.
fn skill_entry_source(
    s: &xai_grok_agent::prompt::skills::SkillInfo,
    grok_home: &Path,
) -> ConfigSource {
    use xai_grok_tools::implementations::skills::types::SkillScope;

    if let Some(source) = s.config_source.clone() {
        return source;
    }
    let path = PathBuf::from(&s.path);
    match s.scope {
        SkillScope::Local | SkillScope::Repo => ConfigSource::Project { path },
        SkillScope::User => {
            if crate::builtin::is_extracted_bundled_skill(&s.name, &path, grok_home) {
                ConfigSource::Bundled { path }
            } else {
                ConfigSource::User { path }
            }
        }
        SkillScope::Server => ConfigSource::Server { path },
        SkillScope::Bundled => ConfigSource::Bundled { path },
        SkillScope::Plugin => ConfigSource::Plugin {
            plugin_name: String::new(),
            path,
        },
    }
}

fn list_agents(
    cwd: &Path,
    plugin_registry: &xai_grok_agent::plugins::PluginRegistry,
) -> Vec<AgentEntry> {
    let agents = xai_grok_agent::discovery::all_subagents_with_plugins(
        cwd,
        &HashMap::new(),
        Some(plugin_registry),
    );

    agents
        .into_iter()
        .map(|a| AgentEntry {
            name: a.name,
            description: a.description,
            source: a.config_source,
        })
        .collect()
}

/// Maps pre-discovered plugins (from `discover_plugins`) to inspect entries.
fn list_plugins(discovered: &[xai_grok_agent::plugins::DiscoveredPlugin]) -> Vec<PluginEntry> {
    discovered
        .iter()
        .map(|p| {
            let scope = match p.scope {
                xai_grok_agent::plugins::PluginScope::CliOverride => Scope::Cli,
                xai_grok_agent::plugins::PluginScope::Project => Scope::Project,
                xai_grok_agent::plugins::PluginScope::User => Scope::User,
                xai_grok_agent::plugins::PluginScope::ConfigPath => Scope::Config,
            };
            PluginEntry {
                name: p.manifest.name.clone(),
                scope,
                path: p.root.display().to_string(),
                enabled: p.trusted,
                provides: PluginProvides {
                    // Count actual SKILL.md files discovered (root-level or in
                    // subdirs), not the number of configured skill dirs, so the
                    // reported count matches what the skills registry loads.
                    skills: xai_grok_agent::plugins::registry::skill_md_paths(&p.skill_dirs).len(),
                    agents: p.agent_dirs.len(),
                    hooks: p.hooks_path.is_some(),
                    mcp_servers: if p.mcp_config_path.is_some() { 1 } else { 0 },
                },
            }
        })
        .collect()
}

/// Wraps the production marketplace resolver (`marketplace::resolve`).
fn list_marketplaces(git_root: Option<&Path>) -> Vec<MarketplaceEntry> {
    let Some(root) = git_root else {
        return vec![];
    };
    xai_grok_agent::plugins::marketplace::resolve(root)
        .into_iter()
        .map(|m| MarketplaceEntry {
            name: m.name,
            path: m.path.display().to_string(),
            enabled_plugins: m.plugin_dirs.len(),
        })
        .collect()
}

/// Discovers MCPs with every vendor enabled so compatibility can be annotated later.
fn list_mcp_servers(
    cwd: &Path,
    plugin_registry: &xai_grok_agent::plugins::PluginRegistry,
) -> Vec<McpServerEntry> {
    use xai_grok_workspace::permission::resolution;

    let all_on = xai_grok_tools::types::compat::CompatConfig::default();
    let sourced = crate::session::managed_mcp::merge_managed_mcp_servers_sourced(
        cwd,
        Some(plugin_registry),
        &all_on,
    );
    let allowlist = &resolution::managed_settings().mcp_allowlist;

    sourced
        .into_iter()
        .map(|(server, source)| {
            let (name, transport, target) =
                match &server {
                    agent_client_protocol::McpServer::Stdio(
                        agent_client_protocol::McpServerStdio { name, command, .. },
                    ) => (name.clone(), "stdio", command.display().to_string()),
                    agent_client_protocol::McpServer::Http(
                        agent_client_protocol::McpServerHttp { name, url, .. },
                    ) => (name.clone(), "http", url.clone()),
                    agent_client_protocol::McpServer::Sse(
                        agent_client_protocol::McpServerSse { name, url, .. },
                    ) => (name.clone(), "sse", url.clone()),
                    // TODO(acp-0.10): `McpServer` is #[non_exhaustive].
                    _ => ("unknown".to_string(), "unknown", String::new()),
                };
            let disabled_reason = (!allowlist.is_server_allowed(&server)).then(|| {
                crate::session::managed_mcp::McpDisabledReason::for_blocked_server(
                    allowlist, &server,
                )
                .to_string()
            });
            let vendor = match &source {
                ConfigSource::ClaudeJson { .. } => Some("claude".to_owned()),
                ConfigSource::McpJson { path } => {
                    derive_vendor(&path.display().to_string()).map(String::from)
                }
                _ => None,
            };
            McpServerEntry {
                name,
                transport: transport.to_string(),
                target,
                source,
                disabled: false,
                compatibility_status: None,
                disabled_reason,
                vendor,
            }
        })
        .collect()
}

/// Wraps the production LSP loader (`load_servers_with_plugins_sourced`).
fn list_lsp_servers(
    cwd: &Path,
    discovered_plugins: &[xai_grok_agent::plugins::DiscoveredPlugin],
) -> Vec<LspServerEntry> {
    let trusted: Vec<_> = discovered_plugins.iter().filter(|p| p.trusted).collect();
    let plugin_lsp_paths: Vec<std::path::PathBuf> = trusted
        .iter()
        .filter_map(|p| p.lsp_config_path.clone())
        .collect();
    let plugin_names: Vec<&str> = trusted
        .iter()
        .filter(|p| p.lsp_config_path.is_some())
        .map(|p| p.manifest.name.as_str())
        .collect();
    let plugin_inline_lsp: Vec<(&serde_json::Value, &str)> = trusted
        .iter()
        .filter_map(|p| {
            p.manifest
                .inline_lsp_servers()
                .map(|v| (v, p.manifest.name.as_str()))
        })
        .collect();
    let inline_values: Vec<&serde_json::Value> =
        plugin_inline_lsp.iter().map(|(v, _)| *v).collect();
    let inline_names: Vec<&str> = plugin_inline_lsp.iter().map(|(_, n)| *n).collect();

    let servers = xai_grok_tools::implementations::lsp::config::load_servers_with_plugins_sourced(
        cwd,
        &plugin_lsp_paths,
        &inline_values,
        &plugin_names,
        &inline_names,
    );

    // Folder-trust gate (display-only): inspect never spawns servers, but mark the
    // repo-local (project-scoped) entries a session would skip in an untrusted
    // clone so the listing matches the live gate. `remote = None` mirrors
    // `grok mcp doctor` (no loaded RemoteSettings in a standalone command).
    crate::agent::folder_trust::resolve_and_record(cwd, None, false);
    let project_allowed = crate::agent::folder_trust::project_scope_allowed(cwd);

    servers
        .into_iter()
        .map(|(name, (cfg, source))| {
            let untrusted = !project_allowed && matches!(source, ConfigSource::Project { .. });
            LspServerEntry {
                name,
                command: cfg.command,
                args: cfg.args,
                source,
                extensions: cfg.extensions.keys().cloned().collect(),
                untrusted,
            }
        })
        .collect()
}

/// Locates the config files that contribute to the effective config by
/// probing the canonical locations used by `ConfigLayers::load` and
/// `requirements_layers`: system + user `managed_config.toml`, user
/// `config.toml`, user + system `requirements.toml`, and project
/// `.grok/config.toml` files (via `find_project_configs`). The macOS MDM
/// managed-preferences layer has no file on disk, so it is sourced directly
/// from `requirements_layers()` rather than a path probe.
///
/// Only on-disk files (plus the synthetic MDM layer) are emitted, except the
/// primary user `config.toml` which always gets a "User: (none)" line in the
/// human view when absent.
/// `note` distinguishes files that exist but contribute nothing after the
/// real loader's processing (stripping, version overrides, fail_closed, etc).
/// Parse errors are reported distinctly rather than as "empty".
fn list_config_sources(cwd: &Path) -> ConfigSources {
    let mut layers: Vec<ConfigLayer> = vec![];

    // System managed (comes first in merge precedence)
    if let Some(dir) = crate::config::system_config_dir() {
        let p = dir.join("managed_config.toml");
        if let Some((path_s, note)) = describe_config_file(&p) {
            layers.push(ConfigLayer {
                role: "system-managed".to_string(),
                path: path_s,
                note,
            });
        }
    }

    // User managed
    if let Some(home) = crate::config::user_grok_home() {
        let p = home.join("managed_config.toml");
        if let Some((path_s, note)) = describe_config_file(&p) {
            layers.push(ConfigLayer {
                role: "managed".to_string(),
                path: path_s,
                note,
            });
        }
    }

    // User config.toml (primary user layer; shown as (none) when absent)
    if let Some(home) = crate::config::user_grok_home() {
        let p = home.join("config.toml");
        if let Some((path_s, note)) = describe_config_file(&p) {
            layers.push(ConfigLayer {
                role: "user".to_string(),
                path: path_s,
                note,
            });
        }
    }

    // Requirements: user then system (order they appear in requirements_layers)
    if let Some(home) = crate::config::user_grok_home() {
        let p = home.join("requirements.toml");
        if let Some((path_s, note)) = describe_requirements_file(&p) {
            layers.push(ConfigLayer {
                role: "requirements".to_string(),
                path: path_s,
                note,
            });
        }
    }
    if let Some(dir) = crate::config::system_config_dir() {
        let p = dir.join("requirements.toml");
        if let Some((path_s, note)) = describe_requirements_file(&p) {
            layers.push(ConfigLayer {
                role: "system-requirements".to_string(),
                path: path_s,
                note,
            });
        }
    }

    // macOS MDM managed preferences: a synthetic, admin-forced requirements layer
    // with no file on disk, so it's sourced from requirements_layers() (keyed on
    // the synthetic label) with contribution decided from the in-memory value
    // rather than a path probe. Absent on non-macOS or when no profile is forced.
    let rt_layers = crate::config::requirements_layers();
    if let Some(mdm) = rt_layers
        .iter()
        .find(|l| matches!(l.source, crate::config::RequirementsSource::Mdm))
    {
        let path_s = mdm.source.label().into_owned();
        let note = if requirements_layer_contributes(&rt_layers, &path_s) {
            None
        } else {
            Some("empty".to_string())
        };
        layers.push(ConfigLayer {
            role: "mdm".to_string(),
            path: path_s,
            note,
        });
    }

    // Project configs (from git root up); each is its own "project" role entry
    for p in crate::config::find_project_configs(cwd) {
        if p.exists()
            && let Some((path_s, note)) = describe_config_file(&p)
        {
            layers.push(ConfigLayer {
                role: "project".to_string(),
                path: path_s,
                note,
            });
        }
    }

    ConfigSources { layers }
}

/// For managed / user / project config files: use `load_config_file` (the
/// production path for those layers) so `note` reflects post-processing
/// (version overrides stripped) and distinguishes parse failure.
fn describe_config_file(path: &Path) -> Option<(String, Option<String>)> {
    if !path.exists() {
        return None;
    }
    let path_s = path.display().to_string();
    match crate::config::load_config_file(path) {
        Ok(v) => {
            let empty = v.as_table().is_none_or(|t| t.is_empty());
            Some((
                path_s,
                if empty {
                    Some("empty".to_string())
                } else {
                    None
                },
            ))
        }
        Err(_) => Some((path_s, Some("parse error".to_string()))),
    }
}

/// Classify a requirements file against the real loader. `load_config_file`
/// catches both syntax errors and invalid `[[version_overrides]]` (the loader
/// rejects the latter too), so those read "(parse error)"; contribution is
/// then sourced from `requirements_layers()` via `requirements_layer_contributes`.
fn describe_requirements_file(path: &Path) -> Option<(String, Option<String>)> {
    if !path.exists() {
        return None;
    }
    let path_s = path.display().to_string();
    if crate::config::load_config_file(path).is_err() {
        return Some((path_s, Some("parse error".to_string())));
    }
    if requirements_layer_contributes(&crate::config::requirements_layers(), &path_s) {
        Some((path_s, None))
    } else {
        Some((path_s, Some("empty".to_string())))
    }
}

/// Whether the loader keeps `path_s` *and* its post-load table is non-empty.
/// The non-empty guard runs before `fail_closed` is stripped, so a
/// `fail_closed`-only file is retained with an empty table yet contributes nothing.
fn requirements_layer_contributes(
    layers: &[crate::config::RequirementsLayer],
    path_s: &str,
) -> bool {
    layers.iter().any(|l| {
        l.source.label().as_ref() == path_s && l.value.as_table().is_some_and(|t| !t.is_empty())
    })
}

fn print_section<T>(title: &str, items: &[T], format_item: impl Fn(&T) -> String) {
    println!();
    println!("  {} ({})", title, items.len());
    if items.is_empty() {
        println!("  {TREE} (none)");
    }
    for item in items {
        println!("  {TREE} {}", format_item(item));
    }
}

/// Print items in a two-column layout: name on the left, source label on the right.
fn print_columns<T>(
    title: &str,
    items: &[T],
    name: impl Fn(&T) -> String,
    label: impl Fn(&T) -> String,
) {
    println!();
    println!("  {} ({})", title, items.len());
    if items.is_empty() {
        println!("  {TREE} (none)");
        return;
    }
    let names: Vec<String> = items.iter().map(&name).collect();
    let pad = names.iter().map(|n| n.len()).max().unwrap_or(0).min(50);
    for (item, n) in items.iter().zip(&names) {
        println!("  {TREE} {:<pad$}  {}", n, label(item));
    }
}

/// Render the team pin for the human view: single value, comma-joined list,
/// or an explicit empty-list marker (which fails closed at login).
fn format_force_login_team(team: &Option<ForceLoginTeam>) -> String {
    match team {
        None => "(none)".to_string(),
        Some(ForceLoginTeam::Single(s)) => s.clone(),
        Some(ForceLoginTeam::AnyOf(list)) if list.is_empty() => {
            "(empty -- fail closed)".to_string()
        }
        Some(ForceLoginTeam::AnyOf(list)) => list.join(", "),
    }
}

/// Human label for an enforced setting. Uses product vocabulary, not the
/// internal field names (no `ui.yolo` / `--yolo` / `permission_mode`).
fn enforced_label(p: &EnforcedPolicy) -> String {
    let name = match p.setting.as_str() {
        "alwaysApprove" => "Permissions mode: always-approve",
        "telemetry" => "Telemetry",
        "feedback" => "Feedback",
        other => other,
    };
    let state = if p.enabled { "enabled" } else { "disabled" };
    format!("{name} {state}")
}

fn disabled_compat_tags(
    disabled: bool,
    compatibility_status: Option<CompatEntryStatus>,
) -> &'static str {
    if disabled || compatibility_status == Some(CompatEntryStatus::Disabled) {
        " [disabled]"
    } else {
        ""
    }
}

/// Renders the "Config Warnings" section of the human report; empty when
/// there are no warnings. Covers `[model.*]` overrides and the
/// `[auth_provider.*]` tables, which share the same warning channel.
fn render_config_warnings(
    warnings: &[crate::agent::config_model_override_parse::ConfigWarning],
) -> String {
    use std::fmt::Write as _;

    if warnings.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n  Config Warnings\n");
    let _ = writeln!(out, "  {TREE} {} warning(s)", warnings.len());
    for w in warnings {
        let field = w.field().map(|f| format!(" {f}")).unwrap_or_default();
        let _ = writeln!(
            out,
            "    {TREE} [{}]{field} — {}",
            w.target.label(),
            w.reason
        );
    }
    out
}

fn render_harness_compatibility(report: &ExternalCompatReport) -> String {
    use std::fmt::Write as _;

    let mut out = String::from("\n  Harness Compatibility\n");
    let mut current_vendor = "";
    for cell in &report.cells {
        if cell.vendor != current_vendor {
            current_vendor = &cell.vendor;
            let _ = writeln!(out, "  {TREE} {current_vendor}");
        }
        let status = if cell.enabled { "on" } else { "OFF" };
        let _ = writeln!(
            out,
            "    {TREE} {:<10} {:<3}  ({})",
            cell.surface, status, cell.source
        );
    }
    out.push('\n');
    out
}

fn print_human(r: &InspectReport) {
    println!();
    println!("  Environment");
    println!("  {TREE} Version: {} [{}]", r.grok_version, r.channel);
    println!("  {TREE} CWD: {}", r.cwd);
    if let Some(ref root) = r.project_root {
        println!("  {TREE} Git root: {}", root);
    }
    println!(
        "  {TREE} Project trusted: {}",
        if r.project_trusted { "yes" } else { "no" }
    );

    print_section("Project Instructions", &r.project_instructions, |f| {
        let status = disabled_compat_tags(f.disabled, f.compatibility_status);
        format!(
            "{} ({}, ~{} tokens){}{}",
            f.path,
            f.scope,
            f.approx_tokens,
            vendor_tag(&f.vendor),
            status,
        )
    });

    println!();
    println!("  Permissions");
    if r.permissions.managed_settings_exists
        && let Some(ref p) = r.permissions.managed_settings_path
    {
        let status = if r.permissions.managed_settings_active {
            "active"
        } else {
            "not loaded"
        };
        println!("  {TREE} Managed settings: {p} ({status})");
    }
    if r.permissions.sources.is_empty() {
        println!("  {TREE} Source: (none)");
    } else {
        for src in &r.permissions.sources {
            println!("  {TREE} Source: {src}");
        }
    }
    println!(
        "  {TREE} {} loaded, {} skipped",
        r.permissions.loaded,
        r.permissions.skipped.len()
    );
    for s in &r.permissions.skipped {
        println!("    {TREE} {} -- {}", s.rule, s.reason);
    }
    if !r.permissions.enforced.is_empty() {
        println!("  {TREE} Enforced by policy");
        for e in &r.permissions.enforced {
            println!("    {TREE} {} ({})", enforced_label(e), e.source);
        }
    }
    if !r.permissions.mcp_server_allowlist.is_empty() {
        println!(
            "  {TREE} MCP server allowlist ({} patterns)",
            r.permissions.mcp_server_allowlist.len()
        );
        for pat in &r.permissions.mcp_server_allowlist {
            println!("    {TREE} {}", pat);
        }
    }
    if !r.permissions.marketplace_allowlist.is_empty() {
        println!(
            "  {TREE} Marketplace allowlist ({} sources)",
            r.permissions.marketplace_allowlist.len()
        );
        for url in &r.permissions.marketplace_allowlist {
            println!("    {TREE} {}", url);
        }
    }

    println!();
    println!("  Login Policy");
    println!(
        "  {TREE} disable_api_key_auth: {}",
        match r.login_policy.disable_api_key_auth {
            Some(v) => v.to_string(),
            None => "(unset)".to_string(),
        }
    );
    println!(
        "  {TREE} force_login_team_uuid: {}",
        format_force_login_team(&r.login_policy.force_login_team_uuid)
    );
    println!(
        "  {TREE} api_key_auth_disabled: {}",
        r.login_policy.api_key_auth_disabled
    );

    print_columns(
        "Skills",
        &r.skills,
        |s| s.name.clone(),
        |s| {
            let status = disabled_compat_tags(s.disabled, s.compatibility_status);
            format!(
                "{}{}{}",
                s.source.display_label(),
                vendor_tag(&s.vendor),
                status,
            )
        },
    );

    print_columns(
        "Agents",
        &r.agents,
        |a| a.name.clone(),
        |a| a.source.display_label(),
    );

    print_columns(
        "Plugins",
        &r.plugins,
        |p| {
            let status = if p.enabled { "enabled" } else { "disabled" };
            format!("{} ({}, {})", p.name, p.scope, status)
        },
        |p| {
            let mut parts = Vec::new();
            if p.provides.skills > 0 {
                parts.push(format!("{} skills", p.provides.skills));
            }
            if p.provides.agents > 0 {
                parts.push(format!("{} agents", p.provides.agents));
            }
            if p.provides.hooks {
                parts.push("hooks".into());
            }
            if p.provides.mcp_servers > 0 {
                parts.push(format!("{} MCPs", p.provides.mcp_servers));
            }
            if parts.is_empty() {
                "-".into()
            } else {
                parts.join(", ")
            }
        },
    );

    print_section("Marketplaces", &r.marketplaces, |m| {
        format!(
            "{} ({}, {} enabled plugins)",
            m.name, m.path, m.enabled_plugins
        )
    });

    if r.mcp_servers.is_empty() {
        println!();
        println!("  MCP Servers (0)");
        println!("  {TREE} (none) \u{2014} see `grok mcp add --help`");
    } else {
        print_columns(
            "MCP Servers",
            &r.mcp_servers,
            |m| {
                if let Some(ref reason) = m.disabled_reason {
                    format!("{} ({}) [BLOCKED: {}]", m.name, m.transport, reason)
                } else {
                    format!("{} ({})", m.name, m.transport)
                }
            },
            |m| {
                let status = disabled_compat_tags(m.disabled, m.compatibility_status);
                format!(
                    "{}{}{}",
                    m.source.display_label(),
                    vendor_tag(&m.vendor),
                    status,
                )
            },
        );
    }

    print_columns(
        "LSP Servers",
        &r.lsp_servers,
        |l| format!("{} ({} {})", l.name, l.command, l.args.join(" ")),
        |l| {
            let untrusted = if l.untrusted { " [untrusted]" } else { "" };
            format!("{}{}", l.source.display_label(), untrusted)
        },
    );

    print_columns(
        "Hooks",
        &r.hooks,
        |h| {
            let matcher = h
                .matcher
                .as_ref()
                .map(|m| format!(" matcher={}", m))
                .unwrap_or_default();
            format!("{}{}", h.hook_type, matcher)
        },
        |h| {
            let status = disabled_compat_tags(h.disabled, h.compatibility_status);
            format!(
                "{}{}{}",
                h.source.display_label(),
                vendor_tag(&h.vendor),
                status,
            )
        },
    );

    println!();
    println!("  Config Sources");
    // User is always emitted (with (none) when absent) for the primary user config.
    if let Some(user_l) = r.config_sources.layers.iter().find(|l| l.role == "user") {
        let tag = match user_l.note.as_deref() {
            Some("empty") => " (empty)",
            Some("parse error") => " (parse error)",
            _ => "",
        };
        println!("  {TREE} User: {}{}", user_l.path, tag);
    } else {
        println!("  {TREE} User: (none)");
    }
    for layer in &r.config_sources.layers {
        if layer.role == "user" {
            continue;
        }
        let tag = match layer.note.as_deref() {
            Some("empty") => " (empty)",
            Some("parse error") => " (parse error)",
            _ => "",
        };
        let label = match layer.role.as_str() {
            "system-managed" => "System Managed",
            "managed" => "Managed",
            "system-requirements" => "System Requirements",
            "requirements" => "Requirements",
            "mdm" => "MDM Requirements",
            "project" => "Project",
            other => other,
        };
        println!("  {TREE} {}: {}{}", label, layer.path, tag);
    }
    if !r.config_sources.layers.iter().any(|l| l.role == "project") {
        println!("  {TREE} Project: (none)");
    }

    print!("{}", render_config_warnings(&r.config_warnings));

    print!("{}", render_harness_compatibility(&r.external_compat));
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_agent::prompt::skills::{SkillInfo, SkillsConfig};
    use xai_grok_tools::implementations::skills::types::SkillScope;

    #[test]
    fn harness_compatibility_human_output_stays_compact() {
        let effective_config: toml::Value =
            toml::from_str("[compat.cursor]\nrules = false").unwrap();
        let report = compat::resolve_inspect_compat_with_env(Ok(&effective_config), |_| None);

        let human = render_harness_compatibility(&report);

        assert!(human.contains("skills     on   (default)"), "{human}");
        assert!(human.contains("rules      OFF  (config)"), "{human}");
        assert!(
            !human.contains("Defaults shown; remote may override."),
            "{human}"
        );
        assert!(!human.contains("resolved at session start"), "{human}");
        assert!(!human.contains("unresolved"), "{human}");
        assert!(!human.contains("?"), "{human}");
    }

    #[test]
    fn disabled_entry_status_serializes_and_renders_consistently() {
        let entry = InstructionFile {
            path: "/repo/.cursor/AGENTS.md".to_owned(),
            scope: Scope::Project,
            file_type: "agents_md".to_owned(),
            size_bytes: 10,
            approx_tokens: 3,
            vendor: Some("cursor".to_owned()),
            disabled: false,
            compatibility_status: Some(CompatEntryStatus::Disabled),
        };
        assert_eq!(
            serde_json::to_value(&entry).unwrap(),
            serde_json::json!({
                "path": "/repo/.cursor/AGENTS.md",
                "scope": "project",
                "fileType": "agents_md",
                "sizeBytes": 10,
                "approxTokens": 3,
                "vendor": "cursor",
                "compatibilityStatus": "disabled"
            })
        );
        assert_eq!(
            disabled_compat_tags(false, entry.compatibility_status),
            " [disabled]"
        );
    }

    #[test]
    fn vendor_rule_paths_select_rules_compatibility_cells() {
        let cell = |vendor: &str, surface: &str, enabled: bool| ExternalCompatEntry {
            vendor: vendor.to_owned(),
            surface: surface.to_owned(),
            enabled,
            source: CompatSource::Config,
        };
        let report = ExternalCompatReport {
            remote_settings_loaded: false,
            cells: vec![
                cell("cursor", "rules", false),
                cell("cursor", "agents", true),
                cell("claude", "rules", false),
                cell("claude", "agents", true),
            ],
        };

        for (vendor, path) in [
            ("cursor", "/repo/.cursor/rules/team.md"),
            ("cursor", r"C:\repo\.cursor\rules\team.md"),
            ("claude", "/repo/.claude/rules/team.md"),
            ("claude", r"C:\repo\.claude\rules\team.md"),
        ] {
            let file_type = instruction_file_type(path, Path::new("/home/user/.grok"), false, &[]);
            assert_eq!(file_type, "rules");
            assert_eq!(
                instruction_compat_status(&Some(vendor.to_owned()), file_type, &report),
                Some(CompatEntryStatus::Disabled)
            );
        }

        for path in ["/repo/.grok/rules/team.md", r"C:\repo\.grok\rules\team.md"] {
            assert_eq!(
                instruction_file_type(path, Path::new("/home/user/.grok"), false, &[]),
                "rules"
            );
        }
        for path in [
            "/repo/.cursor/rules/team.md",
            r"C:\repo\.cursor\rules\team.md",
        ] {
            assert_eq!(
                instruction_file_type(path, Path::new("/home/user/.grok"), true, &[]),
                "rules"
            );
        }
        for path in [
            "/repo/.claude/rules/team.md",
            r"C:\repo\.claude\rules\team.md",
        ] {
            let file_type = instruction_file_type(path, Path::new("/home/user/.grok"), true, &[]);
            assert_eq!(file_type, "agents_md");
            assert_eq!(
                instruction_compat_status(&Some("claude".to_owned()), file_type, &report),
                Some(CompatEntryStatus::Enabled)
            );
        }
        for path in [
            "/repo/not.cursor/rules/team.md",
            r"C:\repo\.cursor\ruleset\team.md",
        ] {
            assert_eq!(
                instruction_file_type(path, Path::new("/home/user/.grok"), false, &[]),
                "agents_md"
            );
        }
    }

    #[test]
    fn grok_home_nested_in_workspace_keeps_direct_surfaces_global() {
        let grok_home = Path::new("/repo/config");
        let workspace = Path::new("/repo");
        for path in ["/repo/config/AGENTS.md", "/repo/config/rules/global.md"] {
            assert!(matches!(
                instruction_scope(path, grok_home, &[], workspace),
                Scope::Global
            ));
        }
        for path in [
            "/repo/config/.grok/rules/project.md",
            "/repo/config/src/AGENTS.md",
        ] {
            assert!(matches!(
                instruction_scope(path, grok_home, &[], workspace),
                Scope::Project
            ));
        }
    }

    #[test]
    fn vendor_home_nested_in_workspace_keeps_direct_surfaces_global() {
        let vendor_homes = vec![(Path::new("/repo/.claude").to_path_buf(), true)];
        let workspace = Path::new("/repo");
        for path in ["/repo/.claude/rules/global.md", "/repo/.claude/CLAUDE.md"] {
            assert!(matches!(
                instruction_scope(path, Path::new("/other/grok"), &vendor_homes, workspace),
                Scope::Global
            ));
        }
        for path in [
            "/repo/.claude/.claude/rules/project.md",
            "/repo/.claude/src/AGENTS.md",
        ] {
            assert!(matches!(
                instruction_scope(path, Path::new("/other/grok"), &vendor_homes, workspace),
                Scope::Project
            ));
        }
    }

    #[test]
    fn workspace_scope_wins_inside_grok_home() {
        let grok_home = Path::new("/custom/grok");
        let workspace = Path::new("/custom/grok/worktrees/repo");
        for path in [
            "/custom/grok/worktrees/repo/.cursor/rules/project.md",
            "/custom/grok/worktrees/repo/src/AGENTS.md",
        ] {
            assert!(matches!(
                instruction_scope(path, grok_home, &[], workspace),
                Scope::Project
            ));
        }
        assert!(matches!(
            instruction_scope("/custom/grok/rules/global.md", grok_home, &[], workspace,),
            Scope::Global
        ));
    }

    #[test]
    fn custom_grok_home_rules_are_classified_as_rules() {
        assert_eq!(
            instruction_file_type(
                "/custom/config/rules/team.md",
                Path::new("/custom/config"),
                false,
                &[],
            ),
            "rules"
        );
        assert_eq!(
            instruction_file_type(
                "/custom/config/AGENTS.md",
                Path::new("/custom/config"),
                false,
                &[],
            ),
            "agents_md"
        );
    }

    #[test]
    fn describe_config_file_flags_empty_and_parse_error() {
        let dir = tempfile::tempdir().unwrap();

        // Missing file: describe returns None (no layer entry).
        let missing = dir.path().join("missing.toml");
        assert!(describe_config_file(&missing).is_none());

        // Comment-only and whitespace-only files parse to an empty table after load.
        let comment_only = dir.path().join("comment.toml");
        std::fs::write(&comment_only, "# nothing enforced here\n").unwrap();
        let (_, note) = describe_config_file(&comment_only).unwrap();
        assert_eq!(note.as_deref(), Some("empty"));

        let blank = dir.path().join("blank.toml");
        std::fs::write(&blank, "\n\n").unwrap();
        let (_, note) = describe_config_file(&blank).unwrap();
        assert_eq!(note.as_deref(), Some("empty"));

        // A file with real content contributes config and has no note.
        let with_content = dir.path().join("content.toml");
        std::fs::write(&with_content, "[telemetry]\nmode = \"disabled\"\n").unwrap();
        let (_, note) = describe_config_file(&with_content).unwrap();
        assert!(note.is_none());

        // Malformed TOML is flagged as parse error (distinct from empty).
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "[[[ this is not valid toml").unwrap();
        let (_, note) = describe_config_file(&bad).unwrap();
        assert_eq!(note.as_deref(), Some("parse error"));
    }

    #[test]
    fn describe_requirements_file_flags_invalid_version_overrides_as_parse_error() {
        // Valid TOML but invalid `[[version_overrides]]` is rejected by the real
        // loader, so it must read "parse error", not "empty".
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("requirements.toml");
        std::fs::write(&path, "[[version_overrides]]\nminimum_version = \"nope\"\n").unwrap();
        let (_, note) = describe_requirements_file(&path).unwrap();
        assert_eq!(note.as_deref(), Some("parse error"));
    }

    #[test]
    fn requirements_layer_contributes_requires_non_empty_post_strip_table() {
        // A `fail_closed`-only file is kept by the loader but with an empty
        // post-strip table, so it must not count as contributing.
        let path = "/home/u/.grok/requirements.toml";
        let layer = |v| crate::config::RequirementsLayer {
            value: v,
            source: crate::config::RequirementsSource::File(std::path::PathBuf::from(path)),
            is_system: false,
        };
        let empty = layer(toml::Value::Table(toml::map::Map::new()));
        assert!(!requirements_layer_contributes(
            std::slice::from_ref(&empty),
            path
        ));

        let mut tbl = toml::map::Map::new();
        tbl.insert("telemetry".into(), toml::Value::Boolean(true));
        let full = layer(toml::Value::Table(tbl));
        assert!(requirements_layer_contributes(
            std::slice::from_ref(&full),
            path
        ));
    }

    #[test]
    fn enforced_label_uses_product_vocabulary() {
        let p = EnforcedPolicy {
            setting: "alwaysApprove".into(),
            enabled: false,
            source: "managed-settings.json".into(),
        };
        assert_eq!(
            enforced_label(&p),
            "Permissions mode: always-approve disabled"
        );
        assert!(!enforced_label(&p).contains("yolo"));
    }

    /// Model-override warnings flow from an effective config through `Config`
    /// to the human renderer and the JSON report.
    #[test]
    fn config_warnings_inspect_smoke() {
        let effective: toml::Value = toml::from_str(
            r#"
            [model."grok-4.5"]
            model = "grok-4.5"
            env_key = "ANTHROPIC_AUTH_TOKEN"
            compactions_remaining = 1
            send_compactions_remaining = true
            reasoning_effort = "not-a-level"
            "#,
        )
        .unwrap();
        let cfg = crate::agent::config::Config::new_from_toml_cfg(&effective).unwrap();
        let warnings = cfg.config_warnings;
        assert!(
            warnings
                .iter()
                .any(|w| w.field() == Some("send_compactions_remaining")),
            "duplicate alias should warn: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.field() == Some("reasoning_effort")),
            "invalid enum should warn: {warnings:?}"
        );
        assert!(cfg.config_models.contains_key("grok-4.5"));

        let human = render_config_warnings(&warnings);
        assert!(human.contains("Config Warnings"), "{human}");
        assert!(
            human.contains("[model.\"grok-4.5\"] send_compactions_remaining"),
            "{human}"
        );
        assert!(
            human.contains("[model.\"grok-4.5\"] reasoning_effort"),
            "{human}"
        );
        // Auth-provider warnings render under their own table syntax.
        let provider_warning =
            crate::agent::config_model_override_parse::ConfigWarning::auth_provider(
                "litellm",
                Some("command"),
                crate::agent::config_model_override_parse::ConfigWarningKind::InvalidValue,
                "missing or empty command".to_owned(),
            );
        let human = render_config_warnings(&[provider_warning]);
        assert!(
            human.contains("[auth_provider.\"litellm\"] command"),
            "{human}"
        );
        // A dotted provider name renders whole; the field splits off the
        // right.
        let dotted = crate::agent::config_model_override_parse::ConfigWarning::auth_provider(
            "corp.gateway",
            Some("token_ttl_secs"),
            crate::agent::config_model_override_parse::ConfigWarningKind::InvalidValue,
            "at or below the refresh margin".to_owned(),
        );
        let human = render_config_warnings(&[dotted]);
        assert!(
            human.contains("[auth_provider.\"corp.gateway\"] token_ttl_secs"),
            "{human}"
        );
        assert_eq!(render_config_warnings(&[]), "");

        let json = serde_json::to_value(&warnings).unwrap();
        let alias_warning = json
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["field"] == "send_compactions_remaining")
            .expect("alias warning present in JSON");
        assert_eq!(alias_warning["target"], "model");
        assert_eq!(alias_warning["key"], "grok-4.5");
        assert_eq!(alias_warning["kind"], "duplicate-alias");
        assert!(
            alias_warning["reason"]
                .as_str()
                .is_some_and(|r| !r.is_empty())
        );
    }

    // ── skill source mapping (skill_entry_source) ─────────────────────────

    fn skill_fixture(name: &str, path: &str, scope: SkillScope) -> SkillInfo {
        SkillInfo {
            name: name.to_string(),
            description: format!("desc for {name}"),
            path: path.to_string(),
            scope,
            ..SkillInfo::default()
        }
    }

    #[test]
    fn skill_entry_source_maps_scopes() {
        let home = Path::new("/home/u/.grok");

        let s = skill_fixture("a", "/repo/.grok/skills/a/SKILL.md", SkillScope::Local);
        assert!(matches!(
            skill_entry_source(&s, home),
            ConfigSource::Project { .. }
        ));

        let s = skill_fixture("b", "/repo/.grok/skills/b/SKILL.md", SkillScope::Repo);
        assert!(matches!(
            skill_entry_source(&s, home),
            ConfigSource::Project { .. }
        ));

        let s = skill_fixture("c", "/home/u/.grok/skills/c/SKILL.md", SkillScope::User);
        assert!(matches!(
            skill_entry_source(&s, home),
            ConfigSource::User { .. }
        ));

        let s = skill_fixture(
            "d",
            "/home/u/.grok/server-skills/d/SKILL.md",
            SkillScope::Server,
        );
        assert!(matches!(
            skill_entry_source(&s, home),
            ConfigSource::Server { .. }
        ));

        let s = skill_fixture("e", "/home/u/.grok/bundled/e/SKILL.md", SkillScope::Bundled);
        assert!(matches!(
            skill_entry_source(&s, home),
            ConfigSource::Bundled { .. }
        ));
    }

    /// Bundled skills are re-labeled `Bundled` only at their exact extraction
    /// path `<grok_home>/skills/<name>/SKILL.md`; a same-named skill anywhere
    /// else keeps its real source.
    #[test]
    fn skill_entry_source_relabels_extracted_bundled_skills() {
        let home = Path::new("/home/u/.grok");

        let s = skill_fixture(
            "help",
            "/home/u/.grok/skills/help/SKILL.md",
            SkillScope::User,
        );
        assert!(matches!(
            skill_entry_source(&s, home),
            ConfigSource::Bundled { .. }
        ));

        // Bundled name in a project dir: stays project.
        let s = skill_fixture("help", "/repo/.grok/skills/help/SKILL.md", SkillScope::Repo);
        assert!(matches!(
            skill_entry_source(&s, home),
            ConfigSource::Project { .. }
        ));

        // Bundled name in a user dir outside <grok_home>/skills: stays user.
        let s = skill_fixture(
            "help",
            "/home/u/other-skills/help/SKILL.md",
            SkillScope::User,
        );
        assert!(matches!(
            skill_entry_source(&s, home),
            ConfigSource::User { .. }
        ));

        // Bundled frontmatter name in a different dir under <grok_home>/skills:
        // not the extracted copy — stays user.
        let s = skill_fixture(
            "help",
            "/home/u/.grok/skills/my-tools/SKILL.md",
            SkillScope::User,
        );
        assert!(matches!(
            skill_entry_source(&s, home),
            ConfigSource::User { .. }
        ));

        // Non-bundled name under <grok_home>/skills: stays user.
        let s = skill_fixture(
            "my-skill",
            "/home/u/.grok/skills/my-skill/SKILL.md",
            SkillScope::User,
        );
        assert!(matches!(
            skill_entry_source(&s, home),
            ConfigSource::User { .. }
        ));
    }

    /// A discovery-stamped `config_source` (plugins, `[skills].paths`) wins
    /// over the scope fallback.
    #[test]
    fn skill_entry_source_prefers_stamped_config_source() {
        let home = Path::new("/home/u/.grok");
        let mut s = skill_fixture("cfg", "/team/skills/cfg/SKILL.md", SkillScope::User);
        s.config_source = Some(ConfigSource::ConfigToml {
            path: PathBuf::from("/team/skills/cfg/SKILL.md"),
        });
        assert!(matches!(
            skill_entry_source(&s, home),
            ConfigSource::ConfigToml { .. }
        ));
    }

    /// `list_skills` must honor the `[skills]` table like the runtime does:
    /// `paths` skills appear (with a `configToml` source), `ignore`d skills
    /// are hidden, and `disabled` skills stay listed but flagged.
    #[tokio::test]
    async fn list_skills_honors_skills_config() {
        let write = |dir: &Path, name: &str| {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: test skill {name}\n---\n\nBody.\n"),
            )
            .unwrap();
        };
        // Test-unique names: discovery also reads this machine's real ~/.grok dirs.
        let extra = tempfile::tempdir().unwrap();
        write(&extra.path().join("inspect-cfg-extra"), "inspect-cfg-extra");
        write(
            &extra.path().join("inspect-cfg-ignored"),
            "inspect-cfg-ignored",
        );

        let cwd = tempfile::tempdir().unwrap();
        let config = SkillsConfig {
            paths: vec![extra.path().to_string_lossy().into_owned()],
            ignore: vec![
                extra
                    .path()
                    .join("inspect-cfg-ignored")
                    .to_string_lossy()
                    .into_owned(),
            ],
            disabled: vec!["inspect-cfg-extra".to_string()],
            ..Default::default()
        };
        let registry = xai_grok_agent::plugins::PluginRegistry::from_discovered(vec![], &[], &[]);

        let entries = list_skills(cwd.path(), &registry, &config).await;

        let extra_entry = entries
            .iter()
            .find(|e| e.name == "inspect-cfg-extra")
            .expect("[skills].paths skill should be listed");
        assert!(
            matches!(extra_entry.source, ConfigSource::ConfigToml { .. }),
            "unexpected source: {:?}",
            extra_entry.source
        );
        assert!(
            extra_entry.disabled,
            "[skills].disabled must flag the entry"
        );
        assert!(
            !entries.iter().any(|e| e.name == "inspect-cfg-ignored"),
            "[skills].ignore must hide the skill"
        );
    }
}
