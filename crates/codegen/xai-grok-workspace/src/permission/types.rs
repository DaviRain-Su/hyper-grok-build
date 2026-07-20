use agent_client_protocol as acp;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
/// A permission event capturing the decision made for a tool call.
/// Used for telemetry to track permission patterns and user behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionEvent {
    /// Tool call ID from the model
    pub tool_id: String,
    /// Name of the tool being executed
    pub tool_name: String,
    /// Type of access requested (read, edit, bash, mcp)
    pub access_kind: String,
    /// Additional context (e.g., file path for edit, command for bash)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_detail: Option<String>,
    /// Whether YOLO mode was enabled when this decision was made
    pub yolo_mode: bool,
    /// Whether this was auto-approved (by YOLO mode or policy rules)
    pub auto_approved: bool,
    /// Whether the user was prompted for this decision
    pub user_prompted: bool,
    /// The final decision (allow, reject)
    pub decision: String,
    /// The user's choice when prompted (allow_once, allow_always, reject_once,
    /// etc.); None on auto/non-prompt decisions. The trigger lives in `decision_reason`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_outcome: Option<String>,
    /// Rejection reason if rejected
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reject_reason: Option<String>,
    /// When this decision was made
    pub timestamp: DateTime<Utc>,
    /// If this permission was requested by a subagent, the subagent's session ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_session_id: Option<String>,
    /// If this permission was requested by a subagent, its type (e.g. "explore").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_type: Option<String>,
    /// If this permission was requested by a subagent, its description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_description: Option<String>,
    /// Effective permission mode governing this decision (not the trigger):
    /// "ask" | "auto" | "always-approve". Hyphenated to match
    /// `config.ui.permission_mode` in the same trace (differs from the telemetry
    /// enum's underscore Mixpanel serde).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// The trigger that produced this decision, distinct from `prompt_outcome`
    /// (which records the user's choice when prompted). Lets a trace show *why*
    /// a request reached a prompt even when `user_prompted=true`. Values:
    /// yolo, policy_allow, policy_deny, policy_ask, auto_fast_path,
    /// auto_classifier_allow, auto_classifier_block, sandbox_auto,
    /// persisted_grant, session_grant, static_allowlist, safe_command,
    /// session_deny, prompt_deny, needs_user, requester_gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_reason: Option<String>,
    /// Elapsed milliseconds from the actor dequeuing this request to the decision
    /// resolving. The timer starts at dequeue, so it excludes time the request
    /// waited in the channel behind others; small for fast auto paths but
    /// non-trivial when an auto classifier side-query runs before the decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_ms: Option<u64>,
    /// Concurrent in-flight permission requests (this one included) at emit time,
    /// counted across the shared handle so overlapping subagent requests show up.
    /// The per-turn "hit yes N times" count is instead the number of
    /// `user_prompted=true` events in the turn, not this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_depth: Option<u32>,
}
/// Identifies the type of client connecting to the agent.
/// Used to determine which permission UI features to enable
/// and which feedback/experiment client type to report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ClientType {
    /// Generic client - show simple permission options with full command text
    #[default]
    #[serde(rename = "generic", alias = "grok-shell", alias = "grok_shell")]
    Generic,
    /// Grok TUI client - show fancy options with interactive bash term selection
    #[serde(rename = "grok-tui", alias = "grok_tui")]
    GrokTUI,
    /// Grok Web client - identified by clientIdentifier "grok-web"
    #[serde(rename = "grok_web")]
    GrokWeb,
    /// Named client (`"nebula"`) — uses the generic permission UI
    #[serde(rename = "nebula")]
    Nebula,
    /// IDE extension client (VS Code and similar) - identified by clientIdentifier "grok-code-extension"
    #[serde(rename = "extension")]
    Extension,
    /// Grok Pager client - TUI-like terminal pager with interactive permission UI.
    /// Treated identically to GrokTUI for permission options (gets bash highlights +
    /// interactive selection). Reports as "pager" for telemetry attribution.
    ///
    /// Accepts both the hyphenated `"grok-pager"` (what the pager actually
    /// sends over the wire, matching `PAGER_CLIENT_TYPE`) and the underscored
    /// `"grok_pager"` form for symmetry with the rest of this enum.
    #[serde(rename = "grok-pager", alias = "grok_pager")]
    GrokPager,
    /// Grok Desktop (Electron) client - identified by clientIdentifier "grok-desktop".
    /// Uses TUI-style bash permission options (primary command extraction + prefix matching)
    /// but without interactive `<`/`>` word selection.
    #[serde(rename = "grok_desktop")]
    Desktop,
}
impl ClientType {
    /// Product token for the `User-Agent` header (e.g. `grok-pager`).
    pub fn user_agent_label(&self) -> &'static str {
        match self {
            Self::Generic => "grok-shell",
            Self::GrokTUI => "grok-tui",
            Self::GrokWeb => "grok-web",
            Self::Nebula => "nebula",
            Self::Extension => "grok-code-extension",
            Self::GrokPager => "grok-pager",
            Self::Desktop => "grok-desktop",
        }
    }
    /// Resolve from ACP `clientIdentifier` string (e.g. `"grok-web"`, `"grok-desktop"`).
    pub fn from_client_identifier(id: Option<&str>) -> Self {
        match id {
            Some("grok-web") => Self::GrokWeb,
            Some("nebula") => Self::Nebula,
            Some("grok-code-extension") => Self::Extension,
            Some("grok-desktop") => Self::Desktop,
            Some("grok-pager") => Self::GrokPager,
            _ => Self::Generic,
        }
    }
    /// Label for feedback reporting and experiment filtering.
    pub fn feedback_label(&self) -> &'static str {
        match self {
            Self::GrokTUI | Self::GrokPager => "tui",
            Self::GrokWeb => "web",
            Self::Nebula => "nebula",
            Self::Extension => "extension",
            Self::Generic => "agent",
            Self::Desktop => "desktop",
        }
    }
}
#[derive(Clone, Debug)]
pub enum AccessKind {
    Read(Option<String>),
    Grep {
        path: Option<String>,
        glob: Option<String>,
    },
    Edit(String),
    Bash(String),
    /// An MCP tool call: the tool name plus its raw JSON args. The args are
    /// carried so the auto-mode classifier (and telemetry) can judge what the
    /// call actually does, not just its name.
    MCPTool {
        name: String,
        input: serde_json::Value,
    },
    WebFetch(String),
    WebSearch(String),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// A policy `ask` rule matched; prompt the user.
    Ask,
    FollowupMessage(String),
    Reject(String),
    /// A policy deny rule matched. Distinguished from `Reject` (user-initiated)
    /// so the caller can return the error to the LLM instead of cancelling
    /// the turn — the agent should see the denial and adapt.
    PolicyDeny(String),
    /// The user cancelled the turn (e.g. Cmd+C during permission prompt).
    /// Distinguished from `Reject` so the caller can return `StopReason::Cancelled`.
    Cancelled,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditPolicy {
    #[default]
    Ask,
    Allow,
    Reject,
}
impl Serialize for EditPolicy {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Self::Ask => "ask",
            Self::Allow => "allow",
            Self::Reject => "reject",
        })
    }
}
impl<'de> Deserialize<'de> for EditPolicy {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = EditPolicy;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("one of: ask, allow, reject")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<EditPolicy, E> {
                match v {
                    "ask" => Ok(EditPolicy::Ask),
                    "allow" => Ok(EditPolicy::Allow),
                    "reject" => Ok(EditPolicy::Reject),
                    other => Err(E::unknown_variant(other, &["ask", "allow", "reject"])),
                }
            }
        }
        deserializer.deserialize_str(V)
    }
}
#[derive(Debug, Clone)]
pub struct EditPathContext {
    pub real_cwd: std::path::PathBuf,
    pub display_cwd: Option<std::path::PathBuf>,
}
#[allow(clippy::large_enum_variant)]
pub enum PermissionCommand {
    Request {
        access: AccessKind,
        tool_call_update: acp::ToolCallUpdate,
        edit_path_context: Option<EditPathContext>,
        respond_to: oneshot::Sender<Decision>,
        /// Session ID originating this request. Used to attribute
        /// permission events to child subagents.
        session_id: Option<String>,
        /// Subagent type if this request is from a child (e.g. "explore").
        subagent_type: Option<String>,
        /// Subagent description if this request is from a child.
        subagent_description: Option<String>,
    },
    /// Set the YOLO mode (auto-approve all permissions)
    SetYoloMode(bool),
    /// Set auto mode (LLM classifier for non-fast-path tools). Mutually
    /// exclusive with YOLO at the handle level; enabling auto clears yolo
    /// and vice versa when applied by the actor.
    SetAutoMode(bool),
    /// Install or replace the permission classifier used in auto mode.
    SetClassifier(Option<std::sync::Arc<dyn super::auto_mode::PermissionClassifier>>),
    /// Recent transcript turns for classifier context (compacted by caller).
    SetClassifierTranscript(Vec<super::auto_mode::ClassifierTurn>),
    /// Project AGENTS.md instructions for classifier context (None clears).
    SetProjectInstructions(Option<String>),
    /// Reset per-tool permission state back to defaults.
    ResetState,
    Shutdown,
}
impl From<&xai_grok_tools::types::ToolInput> for AccessKind {
    fn from(input: &xai_grok_tools::types::ToolInput) -> Self {
        use xai_grok_tools::types::ToolInput;
        match input {
            ToolInput::ReadFile(r) => AccessKind::Read(Some(r.path.clone())),
            ToolInput::ListDir(l) => AccessKind::Read(Some(l.target_directory.clone())),
            ToolInput::Grep(g) => AccessKind::Grep {
                path: g.path.clone(),
                glob: g.glob.clone(),
            },
            ToolInput::TodoWrite(_)
            | ToolInput::TaskOutput(_)
            | ToolInput::WaitTasks(_)
            | ToolInput::KillTask(_)
            | ToolInput::Skill(_) => AccessKind::Read(None),
            ToolInput::WebSearch(ws) => AccessKind::WebSearch(ws.query.clone()),
            ToolInput::SearchReplace(search_replace) => {
                AccessKind::Edit(search_replace.file_path.to_string())
            }
            ToolInput::ApplyPatch(_) => AccessKind::Edit("apply_patch".to_string()),
            ToolInput::HashlineEdit(he) => AccessKind::Edit(he.file_path.to_string()),
            ToolInput::Write(w) => AccessKind::Edit(w.file_path.clone()),
            ToolInput::Bash(bash) => AccessKind::Bash(bash.command.to_string()),
            ToolInput::Monitor(m) => AccessKind::Bash(m.command.clone()),
            ToolInput::MCPTool(mcp) => AccessKind::MCPTool {
                name: mcp.tool_name.to_string(),
                input: mcp.tool_input.clone(),
            },
            ToolInput::UseTool(u) => AccessKind::MCPTool {
                name: u.tool_name.clone(),
                input: u.tool_input.clone(),
            },
            ToolInput::WebFetch(wf) => AccessKind::WebFetch(wf.url.clone()),
            ToolInput::Dynamic(_) => AccessKind::Read(None),
            #[allow(unreachable_patterns)]
            _ => AccessKind::Read(None),
        }
    }
}
/// Permission policy configuration (duplicated from util/config.rs for Phase 1 move independence; identical).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PermissionConfig {
    pub rules: Vec<PermissionRule>,
    /// What to do when no rule or pre-decision resolves a tool call.
    #[serde(default)]
    pub prompt_policy: PromptPolicy,
}
impl PermissionConfig {
    pub fn new(rules: Vec<PermissionRule>) -> Self {
        Self {
            rules,
            prompt_policy: PromptPolicy::Ask,
        }
    }
}
/// What to do when the permission manager would normally prompt the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptPolicy {
    /// Prompt the user for approval (default).
    #[default]
    Ask,
    /// Deny without prompting (`permissions.defaultMode: "dontAsk"`).
    Deny,
    /// Use the auto-mode classifier (`permissions.defaultMode: "auto"`).
    /// Seeded into the permission manager's auto flag at session start.
    Auto,
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
    Domain,
}
/// Action to take when rule matches.
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
pub enum ToolFilter {
    #[default]
    Any,
    Bash,
    Edit,
    Read,
    Grep,
    Mcp,
    WebFetch,
    WebSearch,
}
/// Where a requirement/permission was loaded from (duplicated for claude_compat).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequirementSource {
    Unknown,
    /// User-writable `~/.grok/requirements.toml` — untrusted for keeping a
    /// catch-all allow under the pin (a restricted user can edit it).
    Requirements {
        path: std::path::PathBuf,
    },
    /// Root-owned system-dir `requirements.toml`. Distinguished at load time
    /// (`RequirementsLayer::is_system`), never inferred from `path`.
    SystemRequirements {
        path: std::path::PathBuf,
    },
    ManagedSettings {
        path: std::path::PathBuf,
    },
    /// Defaults tier; never an admin source.
    ManagedConfig {
        path: std::path::PathBuf,
    },
    Config {
        path: std::path::PathBuf,
    },
    Settings {
        path: std::path::PathBuf,
    },
}
impl std::fmt::Display for RequirementSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => f.write_str("<unknown>"),
            Self::Requirements { path } => write!(f, "{} (requirements)", path.display()),
            Self::SystemRequirements { path } => {
                write!(f, "{} (system requirements)", path.display())
            }
            Self::ManagedSettings { path } => {
                write!(f, "{} (managed-settings)", path.display())
            }
            Self::ManagedConfig { path } => {
                write!(f, "{} (managed config)", path.display())
            }
            Self::Config { path } => write!(f, "{} (config)", path.display()),
            Self::Settings { path } => write!(f, "{} (settings)", path.display()),
        }
    }
}
/// A value paired with its source (duplicated).
#[derive(Debug, Clone)]
pub struct Sourced<T> {
    pub value: T,
    pub source: RequirementSource,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn permission_event_subagent_fields_default_to_none() {
        let json = r#"{
            "tool_id": "tc1",
            "tool_name": "bash",
            "access_kind": "bash",
            "yolo_mode": false,
            "auto_approved": false,
            "user_prompted": true,
            "decision": "allow",
            "timestamp": "2026-03-24T00:00:00Z"
        }"#;
        let event: PermissionEvent = serde_json::from_str(json).unwrap();
        assert!(event.subagent_session_id.is_none());
        assert!(event.subagent_type.is_none());
        assert!(event.subagent_description.is_none());
        assert!(event.permission_mode.is_none());
        assert!(event.decision_reason.is_none());
        assert!(event.wait_ms.is_none());
        assert!(event.queue_depth.is_none());
    }
    #[test]
    fn permission_event_with_subagent_attribution() {
        let event = PermissionEvent {
            tool_id: "tc1".into(),
            tool_name: "bash".into(),
            access_kind: "bash".into(),
            access_detail: None,
            yolo_mode: false,
            auto_approved: false,
            user_prompted: true,
            decision: "allow".into(),
            prompt_outcome: None,
            reject_reason: None,
            timestamp: Utc::now(),
            subagent_session_id: Some("child-1".into()),
            subagent_type: Some("explore".into()),
            subagent_description: Some("Find endpoints".into()),
            permission_mode: Some("ask".into()),
            decision_reason: Some("needs_user".into()),
            wait_ms: Some(1234),
            queue_depth: Some(3),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["subagent_session_id"], "child-1");
        assert_eq!(json["subagent_type"], "explore");
        assert_eq!(json["subagent_description"], "Find endpoints");
        assert_eq!(json["permission_mode"], "ask");
        assert_eq!(json["decision_reason"], "needs_user");
        assert_eq!(json["wait_ms"], 1234);
        assert_eq!(json["queue_depth"], 3);
    }
    #[test]
    fn permission_event_skips_none_optional_fields() {
        let event = PermissionEvent {
            tool_id: "tc1".into(),
            tool_name: "bash".into(),
            access_kind: "bash".into(),
            access_detail: None,
            yolo_mode: false,
            auto_approved: true,
            user_prompted: false,
            decision: "allow".into(),
            prompt_outcome: None,
            reject_reason: None,
            timestamp: Utc::now(),
            subagent_session_id: None,
            subagent_type: None,
            subagent_description: None,
            permission_mode: None,
            decision_reason: None,
            wait_ms: None,
            queue_depth: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("subagent_session_id"));
        assert!(!json.contains("subagent_type"));
        assert!(!json.contains("permission_mode"));
        assert!(!json.contains("decision_reason"));
        assert!(!json.contains("wait_ms"));
        assert!(!json.contains("queue_depth"));
    }
    #[test]
    fn hashline_edit_maps_to_edit_access() {
        use xai_grok_tools::implementations::grok_build_hashline::edit::types::HashlineEditInput;
        use xai_grok_tools::types::ToolInput;
        let input = ToolInput::HashlineEdit(HashlineEditInput {
            file_path: "src/main.rs".into(),
            edits: vec![],
        });
        let access = AccessKind::from(&input);
        assert!(
            matches!(access, AccessKind::Edit(ref p) if p == "src/main.rs"),
            "HashlineEdit should produce AccessKind::Edit with the file path, got {access:?}"
        );
    }
    #[test]
    fn bash_maps_to_bash_access() {
        use xai_grok_tools::implementations::grok_build::bash::BashToolInput;
        use xai_grok_tools::types::ToolInput;
        let input = ToolInput::Bash(BashToolInput {
            command: "cargo test".into(),
            timeout: None,
            description: "run tests".into(),
            is_background: false,
        });
        let access = AccessKind::from(&input);
        assert!(
            matches!(access, AccessKind::Bash(ref cmd) if cmd == "cargo test"),
            "Bash should produce AccessKind::Bash with the command, got {access:?}"
        );
    }
    #[test]
    fn use_tool_maps_to_mcp_tool_access() {
        use xai_grok_tools::implementations::use_tool::UseToolInput;
        use xai_grok_tools::types::ToolInput;
        let input = ToolInput::UseTool(UseToolInput {
            tool_name: "linear__save_issue".into(),
            tool_input: serde_json::json!({ "title" : "test" }),
        });
        let access = AccessKind::from(&input);
        assert!(
            matches!(access, AccessKind::MCPTool { ref name, ref input }
if name ==
            "linear__save_issue" && input["title"] == "test"),
            "UseTool should produce AccessKind::MCPTool carrying the inner tool name and args, got {access:?}"
        );
    }
    #[test]
    fn monitor_maps_to_bash_access() {
        use xai_grok_tools::implementations::grok_build::monitor::types::MonitorInput;
        use xai_grok_tools::types::ToolInput;
        let input = ToolInput::Monitor(MonitorInput {
            command: "tail -f /var/log/syslog".into(),
            description: "watch syslog".into(),
            timeout_ms: None,
            persistent: None,
        });
        let access = AccessKind::from(&input);
        assert!(
            matches!(access, AccessKind::Bash(ref cmd) if cmd ==
            "tail -f /var/log/syslog"),
            "Monitor runs shell and must map to AccessKind::Bash (not Read), got {access:?}"
        );
    }
    #[test]
    fn search_replace_maps_to_edit_access() {
        use xai_grok_tools::implementations::grok_build::search_replace::SearchReplaceInput;
        use xai_grok_tools::types::ToolInput;
        let input = ToolInput::SearchReplace(SearchReplaceInput {
            file_path: "lib.rs".into(),
            old_string: "old".into(),
            new_string: "new".into(),
            replace_all: false,
        });
        let access = AccessKind::from(&input);
        assert!(
            matches!(access, AccessKind::Edit(ref p) if p == "lib.rs"),
            "SearchReplace should produce AccessKind::Edit, got {access:?}"
        );
    }
    #[test]
    fn web_fetch_maps_to_web_fetch_access() {
        use xai_grok_tools::implementations::grok_build::web_fetch::WebFetchInput;
        use xai_grok_tools::types::ToolInput;
        let input = ToolInput::WebFetch(WebFetchInput {
            url: "https://custom.example.com/api".into(),
        });
        let access = AccessKind::from(&input);
        assert!(
            matches!(access, AccessKind::WebFetch(ref u) if u ==
            "https://custom.example.com/api"),
            "WebFetch should produce AccessKind::WebFetch with the URL, got {access:?}"
        );
    }
    #[test]
    fn web_search_maps_to_web_search_access() {
        use xai_grok_tools::implementations::grok_build::web_search::WebSearchInput;
        use xai_grok_tools::types::ToolInput;
        let input = ToolInput::WebSearch(WebSearchInput {
            query: "rust lang".into(),
            allowed_domains: None,
        });
        let access = AccessKind::from(&input);
        assert!(
            matches!(access, AccessKind::WebSearch(ref q) if q == "rust lang"),
            "WebSearch should produce AccessKind::WebSearch with the query, got {access:?}"
        );
    }
    #[test]
    fn apply_patch_maps_to_edit_access() {
        use xai_grok_tools::implementations::codex::apply_patch::ApplyPatchInput;
        use xai_grok_tools::types::ToolInput;
        let input = ToolInput::ApplyPatch(ApplyPatchInput {
            patch: String::new(),
        });
        let access = AccessKind::from(&input);
        assert!(
            matches!(access, AccessKind::Edit(_)),
            "ApplyPatch should produce AccessKind::Edit, got {access:?}"
        );
    }
    #[test]
    fn write_tool_maps_to_edit_access() {
        use xai_grok_tools::implementations::opencode::write::WriteInput;
        use xai_grok_tools::types::ToolInput;
        let input = ToolInput::Write(WriteInput {
            file_path: "/tmp/secret.txt".into(),
            content: "overwritten".into(),
        });
        let access = AccessKind::from(&input);
        assert!(
            matches!(access, AccessKind::Edit(ref p) if p == "/tmp/secret.txt"),
            "Write should produce AccessKind::Edit with the file path, got {access:?}"
        );
    }
    #[test]
    fn client_type_deserializes_grok_shell_as_generic() {
        assert_eq!(
            serde_json::from_value::<ClientType>("grok-shell".into()).unwrap(),
            ClientType::Generic,
        );
        assert_eq!(
            serde_json::from_value::<ClientType>("grok_shell".into()).unwrap(),
            ClientType::Generic,
        );
        assert_eq!(
            serde_json::from_value::<ClientType>("generic".into()).unwrap(),
            ClientType::Generic,
        );
    }
}
