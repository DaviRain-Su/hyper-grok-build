//! Stub surface when the deploy feature is off.

/// Placeholder config — deploy is unavailable in this build.
#[derive(Debug, Clone, Default)]
pub enum AppBuilderDeployerConfig {
    #[default]
    Disabled,
}

impl AppBuilderDeployerConfig {
    pub fn is_enabled(&self) -> bool {
        false
    }
}

pub const DEPLOY_APP_TOOL_NAME: &str = "deploy_app";
