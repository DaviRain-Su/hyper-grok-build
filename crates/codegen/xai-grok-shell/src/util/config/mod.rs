// `McpOAuthConfig` / `McpOAuthConfigMap` re-exported via `mcp` (see `mcp.rs`).

mod announcements;
mod campaigns;
mod hints;
mod load;
mod mcp;
mod permissions;
mod persist;
mod resolve;
mod settings_writes;
mod tips;
mod worktree;

pub use announcements::*;
pub use campaigns::{
    load_effective_config, load_effective_config_disk_only, persist_models_default,
    remote_campaigns_from_settings, set_remote_campaigns_from_settings, sync_campaign_fields,
};
pub use hints::*;
pub use load::*;
pub use mcp::*;
pub use permissions::*;
pub use persist::*;
// `remote` extracted to the `xai-grok-config-types` crate (dependency inversion);
// re-exported so `crate::util::config::{RemoteSettings, GoalRoleModel}` keep working.
pub use resolve::*;
pub use settings_writes::*;
pub use tips::*;
pub use worktree::*;
pub use xai_grok_config_types::{
    CampaignOverride, ContextualHintsRemote, DisplayRefreshSettings, DoomLoopRecoverySettings,
    GoalRoleModel, RemoteSettings, WorktreeAutoGcSettings, WorktreeKindMaxAge,
};
