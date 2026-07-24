//! Config file loading for Grok.
//!
//! Merge order (lowest → highest priority):
//! 1. `/etc/grok/managed_config.toml`
//! 2. `$GROK_HOME/managed_config.toml`
//! 3. `$GROK_HOME/config.toml`
//! 4. `$GROK_HOME/requirements.toml` (cloud cache; Ed25519-signed at rest once a
//!    key is embedded — see [`signed_policy`] — below the OS-protected layers)
//! 5. `/etc/grok/requirements.toml`
//! 6. macOS MDM managed preferences (`ai.x.grok`, admin-forced) — macOS only
//!
//! Each layer applies its own [`[[version_overrides]]`](version_overrides)
//! before merge. Requirements layers (#4–#6) may opt into fail-closed startup;
//! see [`validate_requirements`].

pub mod campaigns;
pub mod config_override;
pub mod fs_atomic;
pub mod global_hook_sources;
mod loader;
mod macos_managed;
mod managed_cache;
pub mod managed_text;
mod paths;
pub mod shell;
pub mod signed_policy;
mod validation;
pub mod version_overrides;

// Only the cross-crate campaign surface is re-exported at the root; the rest stays
// reachable via the `pub mod` paths for in-crate use without widening the API.
pub use campaigns::{
    CampaignEntry, CampaignOverrides, filter_active_campaigns, ids_touching_paths,
};
pub use global_hook_sources::{
    GlobalHookSource, GlobalHookSourceError, GlobalHookSourceKind, ResolvedGlobalHookSources,
    ensure_grok_hook_slots, existing_ancestor_chain, is_direct_hook_json_name,
    list_direct_hook_json_files, missing_configured_sources, path_has_symlink_component,
    resolve_global_hook_sources, unique_ancestors_rootward,
};

#[cfg(unix)]
pub use global_hook_sources::{
    validate_direct_hook_json_file, validated_hook_json_files_for_sources,
};
pub use loader::{
    CampaignsState, ConfigLayers, MANAGED_CONFIG_FILENAME, ManagedConfigLayer,
    REQUIREMENTS_FILENAME, apply_version_overrides_with_registered, campaigns_application_disabled,
    campaigns_state_path, deep_merge_toml, expand_env_vars_in_string, expand_env_vars_in_toml,
    load_config_file, load_dismissed_ids_from_home, load_effective_config_disk_only,
    load_from_disk, load_managed_config, load_system_managed_config, load_toml_file,
    managed_config_layers, managed_config_layers_at, toml_error_detail,
};
pub use macos_managed::MDM_REQUIREMENTS_SOURCE;
pub use managed_cache::{
    MANAGED_CONFIG_CACHE_FILE, ServingIdentity, SyncMarker, bump_rollback_floor,
    bump_rollback_floor_with_now, confirmed_team_switch, confirmed_team_switch_at,
    fail_closed_policy_armed_at, is_managed_config_hard_stale_for, is_managed_config_stale_for,
    managed_config_identity_changed_at, managed_deployment_id, managed_policy_compromised_for,
    mark_managed_config_synced, mark_managed_config_synced_at, normalize_identity,
};
pub use paths::{
    claude_managed_settings_path, claude_managed_settings_probe_path, decode_cwd_from_dirname,
    default_grok_home, encode_cwd_dirname, ensure_sessions_cwd_dir, grok_application,
    grok_application_in, grok_home, sessions_cwd_dir, system_config_dir, user_grok_home,
};
pub use validation::{
    RequirementsError, RequirementsLayer, RequirementsSource, load_merged_requirements,
    requirements_layers, validate_requirements,
};
pub use version_overrides::{VersionOverrideError, apply_version_overrides};

/// Parse an env var as a boolean. `None` if unset or unrecognized.
pub fn env_bool(name: &str) -> Option<bool> {
    let value = std::env::var(name).ok()?;
    match value.trim().to_ascii_lowercase().as_str() {
        "" => None,
        "1" | "true" | "yes" | "on" | "enabled" => Some(true),
        "0" | "false" | "no" | "off" | "disabled" => Some(false),
        _ => None,
    }
}
