pub mod auto_mode;
pub mod bash_command_splitting;
pub mod claude_settings;
mod exec_risk;
mod gate_preflight;
mod hub_permission;
mod manager;
mod policy;
mod prompter;
pub mod resolution;
pub mod rules;
mod shell_access;
mod state;
pub mod types;

pub use auto_mode::{
    AUTO_MODE_CLASSIFIER_SYSTEM_PROMPT, AutoFastPath, CLASSIFIER_TURN_MAX_LEN, ClassifierContext,
    ClassifierFailure, ClassifierMessage, ClassifierMessageRole, ClassifierOutcome,
    ClassifierPromptType, ClassifierSource, ClassifierTurn, ClassifierVerdict, ClassifyTextChannel,
    ClassifyTextFn, FixedClassifier, HeuristicPermissionClassifier, LlmPermissionClassifier,
    PermissionClassifier, SharedClassifier, access_requires_user_interaction, auto_mode_fast_path,
    build_classifier_messages, classifier_output_json_schema, default_auto_mode_classifier,
    is_auto_mode_allowlisted_access, is_auto_mode_allowlisted_tool_name,
    parse_classifier_model_output, parse_classifier_model_text, permission_decision_args,
};
pub use hub_permission::{
    PermissionHookTransport, ToolServerPermissionTransport, access_kind_for_hub_tool,
    hitl_permission_live_enabled, prompt_outcome_allows, request_permission_via_hub,
};

/// Zero-init this module's metric families. See [`crate::init_metrics`].
pub(crate) fn init_metrics() {
    hub_permission::init_metrics();
}
pub use manager::{
    PermissionHandle, default_always_allow_scope, spawn_permission_manager,
    spawn_permission_manager_with_hub,
};
pub use policy::CompiledPolicy;
pub use prompter::{
    ALLOW_EDITS_SESSION_OPTION_ID, AcpPrompter, BashCommandPermission, BashCommandSelectedTerms,
    ENABLE_ALWAYS_APPROVE_OPTION_ID, MCP_TOOL_NAME_DELIMITER, McpScopeSelection, McpToolPermission,
    PromptOutcome, is_enable_always_approve_option, mcp_pretty_name_if_qualified,
    mcp_titleize_segment, mcp_tool_action, mcp_tool_display_name,
};
pub use state::PermissionState;
pub use state::cleanup_stale_permission_state;
pub use types::{AccessKind, ClientType, Decision, PermissionCommand, PermissionEvent};
