//! Permission-policy config value types, extracted from xai-grok-shell
//! (config dependency inversion).

use serde::{Deserialize, Serialize};

/// Permission policy configuration loaded from `[permission]` section in config.toml.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PermissionConfig {
    pub rules: Vec<PermissionRule>,
}

/// A single permission rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRule {
    pub action: RuleAction,
    #[serde(default)]
    pub tool: ToolFilter,
    pub pattern: Option<String>,
    #[serde(default)]
    pub pattern_mode: PatternMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PatternMode {
    #[default]
    Glob,
    /// Match against URL host rather than full string (from `WebFetch(domain:...)`).
    Domain,
}

/// Action to take when rule matches.
///
/// CWE-1188: Default changed from Allow to Deny so that omitting the
/// `action` field in a TOML permission rule does not silently create a
/// catch-all allow rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    Allow,
    #[default]
    Deny,
    Ask,
}

/// Tool filter for permission rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ToolFilter {
    #[default]
    Any,
    Bash,
    Edit,
    Read,
    Grep,
    Mcp,
    WebFetch,
    #[serde(rename = "agent_message", alias = "agentmessage")]
    AgentMessage,
}

#[cfg(test)]
mod tests {
    use super::ToolFilter;

    #[test]
    fn agent_message_wire_round_trips_and_unknown_is_rejected() {
        let filter: ToolFilter = serde_json::from_str(r#""agent_message""#).unwrap();
        assert_eq!(filter, ToolFilter::AgentMessage);
        assert_eq!(
            serde_json::to_string(&filter).unwrap(),
            r#""agent_message""#
        );
        assert!(serde_json::from_str::<ToolFilter>(r#""future_tool""#).is_err());
    }
}
