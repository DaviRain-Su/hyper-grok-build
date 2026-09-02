//! Managed MCP + plugin + marketplace policy engine: `managed-settings.json`
//! plus every `managed_config.toml` / `requirements.toml` layer, resolved
//! strictest-wins into MCP/marketplace allowlists and tighten-only pins.
//!
//! Extracted move-only from `permission::resolution`, which re-exports the
//! public surface so existing `resolution::` paths keep working.

// Transitional shims for pre-origin-aware callers; deleted in the
// enforcement PR stacked on this one.
pub(super) mod compat;
mod layer;
mod marketplace;
mod mcp;
mod parse;
mod url_match;
mod verdict;

pub use layer::{PolicyLayerOwnership, PolicyPin, PolicySourceAuthority};
pub use marketplace::{
    ManagedMarketplace, ManagedMarketplaceKind, MarketplaceAllowlist, MarketplacePolicy,
    normalize_git_url,
};
pub use mcp::{AllowedMcpServer, McpServerAllowlist, McpServerPolicy, PolicySubjectOrigin};
pub use parse::MANAGED_POLICY_CONFIG_KEYS;
pub use verdict::{McpBlockReason, McpSubject, McpVerdict};

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tracing::{info, warn};

use layer::{PolicyLayer, PolicyLayerTier};
use parse::{
    McpPolicyList, parse_extra_marketplaces, parse_mcp_entry_list, parse_strict_marketplaces,
    policy_bool,
};

use super::resolution::parse_managed_settings_base;
use crate::permission::rules::DefaultPermissionMode;
use crate::permission::types::{PermissionRule, Sourced};

/// Managed MCP + plugin + marketplace policy from Claude `managed-settings.json`
/// ([`xai_grok_config::claude_managed_settings_path`]; no `managed-settings.d/`,
/// MDM plist, or registry delivery yet) plus every `managed_config.toml` /
/// `requirements.toml` layer. Strictest-wins: any deny wins, every restricted
/// source must allow, pins only tighten. Loaded once per process.
#[derive(Debug, Default)]
pub struct ManagedSettings {
    pub features: ManagedSettingsFeatures,
    pub permissions: Vec<Sourced<PermissionRule>>,
    /// Parsed `permissions.defaultMode` (highest mode precedence over user files).
    /// Read and populated by the resolution side ([`parse_managed_settings_base`]).
    pub(in crate::permission) default_mode: Option<DefaultPermissionMode>,
    pub mcp_allowlist: McpServerPolicy,
    pub marketplace_allowlist: MarketplacePolicy,
    /// `enableAllProjectMcpServers = false`: drop project MCP unless allowlisted.
    pub project_mcp: PolicyPin,
    /// `plugin_auto_update = false`: no session-start plugin auto-update.
    pub plugin_auto_update: PolicyPin,
    /// Marketplaces pinned via managed `extraKnownMarketplaces`.
    pub extra_marketplaces: Vec<ManagedMarketplace>,
}

static MANAGED_SETTINGS: OnceLock<ManagedSettings> = OnceLock::new();

pub fn managed_settings() -> &'static ManagedSettings {
    MANAGED_SETTINGS.get_or_init(load_managed_settings)
}

fn load_managed_settings() -> ManagedSettings {
    let claude = xai_grok_config::claude_managed_settings_path()
        .and_then(|path| read_managed_settings_json(&path).map(|json| (json, path)));
    let toml_layers = managed_toml_policy_layers(
        xai_grok_config::managed_config_layers(),
        xai_grok_config::requirements_layers(),
    );
    resolve_managed_settings(claude, toml_layers)
}

/// Tier-tag the on-disk TOML layers. Split from [`load_managed_settings`] so a
/// test can feed real fixture files through `managed_config_layers_at` and
/// prove layer discovery still reaches the policy engine.
fn managed_toml_policy_layers(
    managed: Vec<xai_grok_config::ManagedConfigLayer>,
    requirements: Vec<xai_grok_config::RequirementsLayer>,
) -> Vec<PolicyLayer> {
    let mut toml_layers: Vec<PolicyLayer> = Vec::new();
    for layer in managed {
        toml_layers.push(PolicyLayer {
            tier: if layer.is_system {
                PolicyLayerTier::SystemManaged
            } else {
                PolicyLayerTier::UserManaged
            },
            path: layer.path,
            value: layer.value,
        });
    }
    for layer in requirements {
        toml_layers.push(PolicyLayer {
            tier: match layer.source {
                xai_grok_config::RequirementsSource::Mdm => PolicyLayerTier::Mdm,
                _ if layer.is_system => PolicyLayerTier::SystemRequirements,
                _ => PolicyLayerTier::UserRequirements,
            },
            path: PathBuf::from(layer.source.label().as_ref()),
            value: layer.value,
        });
    }
    toml_layers
}

/// Pure form of [`load_managed_settings`] over pre-loaded sources (testable
/// without on-disk config). Native TOML layers apply trust-descending (see
/// [`PolicyLayerTier`]); the advisory Claude file applies last, so its extras
/// and pin attributions never claim a name ahead of a grok layer.
fn resolve_managed_settings(
    claude: Option<(serde_json::Value, PathBuf)>,
    mut toml_layers: Vec<PolicyLayer>,
) -> ManagedSettings {
    let mut ms = match &claude {
        Some((json, path)) => parse_managed_settings_base(json, path),
        None => ManagedSettings::default(),
    };
    toml_layers.sort_by_key(|layer| layer.tier);
    for layer in toml_layers {
        // Only the policy keys cross TOML → JSON (one parser serves both
        // surfaces). Filtering first keeps an unrelated exotic value elsewhere
        // in the layer (TOML `inf`/`nan` floats have no JSON form) from
        // discarding the layer's policy pins wholesale.
        let policy_table: toml::map::Map<String, toml::Value> = layer
            .value
            .as_table()
            .map(|t| {
                t.iter()
                    .filter(|(k, _)| MANAGED_POLICY_CONFIG_KEYS.contains(&k.as_str()))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default();
        if policy_table.is_empty() {
            continue;
        }
        match serde_json::to_value(&policy_table) {
            Ok(json) => apply_policy_source(&mut ms, &json, &layer.path, layer.tier),
            Err(e) => {
                tracing::error!(
                    path = %layer.path.display(),
                    error = %e,
                    "policy layer could not be read; its MCP/plugin/marketplace pins are NOT applied"
                );
            }
        }
    }
    if let Some((json, path)) = &claude {
        // Already JSON, applied unfiltered; Vendor sorts last, so the apply
        // order matches the sorted loop.
        apply_policy_source(&mut ms, json, path, PolicyLayerTier::Vendor);
    }
    ms
}

/// [`parse_managed_settings_base`] plus the file's advisory policy — the
/// single-source (Claude-only) parse used by tests; the layered runtime path
/// is [`resolve_managed_settings`], which applies the advisory policy after
/// every native TOML layer.
#[cfg(test)]
fn parse_managed_settings_json(json: &serde_json::Value, path: &Path) -> ManagedSettings {
    let mut ms = parse_managed_settings_base(json, path);
    apply_policy_source(&mut ms, json, path, PolicyLayerTier::Vendor);
    ms
}

/// Apply a tighten-only disable pin (first pinning layer names the source).
fn pin_disabled(pin: &mut PolicyPin, path: &Path) {
    if !pin.is_disabled() {
        *pin = PolicyPin::Disabled {
            source: path.to_path_buf(),
        };
    }
}

/// Fold one source's policy pins (Claude JSON or a JSON-ified TOML layer) into
/// `ms` strictest-wins: sources only accumulate, so a later layer can add
/// restrictions but never remove another layer's. The tier derives the
/// source's authority — how its MCP/marketplace restrictions bind (pins are
/// authority-blind) — and ownership (who can write the layer).
fn apply_policy_source(
    ms: &mut ManagedSettings,
    json: &serde_json::Value,
    path: &Path,
    tier: PolicyLayerTier,
) {
    let authority = tier.authority();
    let ownership = tier.ownership();
    let mcp_allow_entries = parse_mcp_entry_list(json, McpPolicyList::Allow);
    let mcp_deny_entries = parse_mcp_entry_list(json, McpPolicyList::Deny);
    let managed_only = policy_bool(
        json,
        "allowManagedMcpServersOnly",
        "allow_managed_mcp_servers_only",
    ) == Some(true);

    if !mcp_allow_entries.is_empty() || !mcp_deny_entries.is_empty() || managed_only {
        info!(
            path = %path.display(),
            allow = mcp_allow_entries.len(),
            deny = mcp_deny_entries.len(),
            managed_only,
            "Loaded MCP server policy"
        );
        let mut allowlist = McpServerAllowlist::new(
            mcp_allow_entries,
            mcp_deny_entries,
            Some(path.to_path_buf()),
        )
        .with_authority(authority);
        if managed_only {
            allowlist = allowlist.with_managed_only();
        }
        ms.mcp_allowlist.sources.push(allowlist);
    }

    if policy_bool(
        json,
        "enableAllProjectMcpServers",
        "enable_all_project_mcp_servers",
    ) == Some(false)
    {
        pin_disabled(&mut ms.project_mcp, path);
    }

    if policy_bool(json, "pluginAutoUpdate", "plugin_auto_update") == Some(false) {
        pin_disabled(&mut ms.plugin_auto_update, path);
    }

    let strict = parse_strict_marketplaces(json);
    if !strict.is_empty() {
        info!(
            path = %path.display(),
            count = strict.len(),
            "Loaded marketplace allowlist"
        );
        ms.marketplace_allowlist.sources.push(MarketplaceAllowlist {
            allowed_urls: strict,
            source_path: Some(path.to_path_buf()),
            authority,
        });
    }

    for (extra, auto_update) in parse_extra_marketplaces(json, ownership) {
        // Claude's per-marketplace `autoUpdate: false` has no granular grok
        // equivalent, so it pins the GLOBAL auto-update off (tighten-only)
        // rather than silently dropping an update opt-out.
        if auto_update == Some(false) {
            pin_disabled(&mut ms.plugin_auto_update, path);
        }
        // First pinning source wins a name (sources apply trust-descending).
        if !ms.extra_marketplaces.iter().any(|m| m.name == extra.name) {
            ms.extra_marketplaces.push(extra);
        }
    }
}

fn read_managed_settings_json(path: &Path) -> Option<serde_json::Value> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "Failed to read managed-settings.json");
            return None;
        }
    };
    match serde_json::from_str(&content) {
        Ok(v) => Some(v),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "Failed to parse managed-settings.json");
            None
        }
    }
}

#[derive(Debug, Default)]
pub struct ManagedSettingsFeatures {
    pub disable_telemetry: Option<bool>,
    pub disable_feedback: Option<bool>,
    pub disable_yolo: Option<bool>,
    pub source_path: Option<std::path::PathBuf>,
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
