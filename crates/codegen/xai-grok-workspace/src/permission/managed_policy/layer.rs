//! Policy layer vocabulary: source tiers, authority, ownership, and
//! tighten-only pins.

use std::path::{Path, PathBuf};

/// Trust tier of a policy layer; lower = higher authority (matching the
/// campaign precedence mdm > system > user, with the vendor Claude file last).
/// Allow/deny accumulation is order-blind, but first-wins resolution — extras
/// name dedupe and pin attribution — applies layers in this order so a
/// user-writable layer or the advisory vendor file can never claim a
/// marketplace name or a pin's attribution ahead of an admin layer.
///
/// A layer's [authority](Self::authority) and [ownership](Self::ownership)
/// are derived from its tier — the tier is the single trust descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum PolicyLayerTier {
    Mdm,
    SystemRequirements,
    SystemManaged,
    UserRequirements,
    UserManaged,
    /// The Claude `managed-settings.json`; sorts (applies) after every grok
    /// layer.
    Vendor,
}

impl PolicyLayerTier {
    /// Vendor files configure Claude, not grok — advisory to grok-native
    /// subjects.
    pub fn authority(self) -> PolicySourceAuthority {
        match self {
            Self::Vendor => PolicySourceAuthority::Advisory,
            _ => PolicySourceAuthority::Native,
        }
    }

    /// Who can write the layer: MDM/system TOML and the root-owned vendor
    /// file are admin-controlled; `~/.grok` layers are user-writable.
    pub fn ownership(self) -> PolicyLayerOwnership {
        match self {
            Self::UserRequirements | Self::UserManaged => PolicyLayerOwnership::User,
            Self::Mdm | Self::SystemRequirements | Self::SystemManaged | Self::Vendor => {
                PolicyLayerOwnership::Admin
            }
        }
    }
}

/// One TOML policy layer. The vendor JSON layer ([`PolicyLayerTier::Vendor`])
/// is applied separately: its value is already JSON and skips the TOML policy
/// key filter.
pub(super) struct PolicyLayer {
    pub(super) tier: PolicyLayerTier,
    pub(super) path: PathBuf,
    pub(super) value: toml::Value,
}

/// Whether a policy source binds every server or is advisory.
///
/// grok's own signed TOML layers (`managed_config.toml`, `requirements.toml`)
/// are [`Native`](Self::Native) and bind everything. The vendor Claude
/// `managed-settings.json` is [`Advisory`](Self::Advisory): hosts ship that
/// file to configure Claude, not grok, so its restrictions bind
/// only subjects grok did not natively define — foreign-sourced MCP servers
/// and marketplaces. Grants are deliberately authority- AND ownership-blind
/// (known limitation; admin-vs-user grant rules are a named follow-up).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PolicySourceAuthority {
    #[default]
    Native,
    Advisory,
}

/// Tighten-only pin: unpinned, or disabled by a named layer (`true` never
/// un-pins).
#[derive(Debug, Clone, Default)]
pub enum PolicyPin {
    #[default]
    Unpinned,
    Disabled {
        source: PathBuf,
    },
}

impl PolicyPin {
    pub fn is_disabled(&self) -> bool {
        matches!(self, Self::Disabled { .. })
    }

    /// The policy layer that pinned this off, if any.
    pub fn source(&self) -> Option<&Path> {
        match self {
            Self::Unpinned => None,
            Self::Disabled { source } => Some(source),
        }
    }
}

/// Who can write the layer a policy value came from: an administrator
/// (MDM, root-owned system TOML, the root-owned Claude managed-settings.json)
/// or the user (`~/.grok` layers). Privileges that carve exceptions out of a
/// lockdown — e.g. a Local marketplace pin surviving a strict list — must
/// require `Admin`, or a user-writable file re-opens the hole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyLayerOwnership {
    Admin,
    User,
}
