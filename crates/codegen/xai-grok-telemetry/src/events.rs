//! Telemetry event structs. Every struct needs a `telemetry_event!` binding.
//! `session_id` and `turn_number` are auto-injected by `log_event` (which
//! lives in shell's integration layer).
//!
//! These structs were extracted from `xai-grok-shell` so they can be
//! reused across binaries (TUI, sampler) without dragging the shell HTTP /
//! Mixpanel client along. The `CompactionScope` helper that drives paired
//! `compaction_triggered`/`compaction_completed` emission stays in shell --
//! it calls `super::log_event` directly.

use serde::Serialize;

use super::enums::PermissionMode;
pub use super::enums::PrCreationSource;

/// Binds a product event name to a struct. Implement via `telemetry_event!` below.
pub trait TelemetryEvent: Serialize + Send + 'static {
    const NAME: &'static str;

    /// Curated external-OTEL representation (see [`crate::external`]).
    /// Default: not exported externally. Override via the macro's
    /// `external = …` arm — the mapping functions live together in
    /// `external/schema.rs` so the whole wire schema is one reviewable file.
    fn external_record(&self) -> Option<crate::external::schema::ExternalRecord> {
        None
    }
}

macro_rules! telemetry_event {
    ($struct:path, $name:literal) => {
        impl $crate::events::TelemetryEvent for $struct {
            const NAME: &'static str = $name;
        }
    };
    ($struct:path, $name:literal, external = $mapper:path) => {
        impl $crate::events::TelemetryEvent for $struct {
            const NAME: &'static str = $name;

            fn external_record(&self) -> Option<$crate::external::schema::ExternalRecord> {
                $mapper(self)
            }
        }
    };
}

// ─────────────────────────────────────────────────────────────────────────────
// Typed enum fields (compile-time exhaustiveness replaces String comments)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum PlanModeTrigger {
    User,
    Tool,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ContextualTipKind {
    Undo,
    PlanMode,
    ImageInput,
    SendNow,
    SmallScreen,
    /// Double-click fold/nav path → tip to enable Word select in settings.
    WordSelect,
    /// SSH session without `grok wrap` → tip to wrap the ssh command locally.
    SshWrap,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ContextualTipAction {
    Shown,
    Accepted,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum PromptSuggestionAction {
    /// A suggestion loaded and rendered as ghost text in the prompt input.
    Shown,
    /// The user accepted the ghost text (Tab / Right arrow).
    Accepted,
    /// The user explicitly dismissed the ghost text (Esc).
    Dismissed,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum YoloTrigger {
    SlashCommand,
    ClientMeta,
    Pager,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum AccessKind {
    Read,
    Edit,
    Bash,
    Grep,
    Mcp,
    Web,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOutcome {
    Allow,
    Deny,
    Cancelled,
    Followup,
}

impl PermissionOutcome {
    /// Stable snake_case label matching the serde representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Cancelled => "cancelled",
            Self::Followup => "followup",
        }
    }
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    Manual,
    Auto,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    Cancelled,
    Error,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum HookOutcome {
    Success,
    Error,
    Blocked,
}

/// Outcome of one `PreToolUse` gate callback. Only `Denied` blocks the tool; the rest
/// (including the `TimedOut`/`TransportError`/`Malformed`/`UnknownDecision` fail-open paths) let it run.
#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ClientHookGateOutcome {
    Denied,
    Proceeded,
    TimedOut,
    TransportError,
    Malformed,
    UnknownDecision,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    Stdio,
    Sse,
    Http,
}

pub use super::enums::McpInitStrategy as McpStrategy;

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum McpErrorType {
    Connection,
    Auth,
    Protocol,
    Timeout,
    SpawnFailed,
    HandshakeFailed,
}

impl McpErrorType {
    /// Stable snake_case label matching the serde representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connection => "connection",
            Self::Auth => "auth",
            Self::Protocol => "protocol",
            Self::Timeout => "timeout",
            Self::SpawnFailed => "spawn_failed",
            Self::HandshakeFailed => "handshake_failed",
        }
    }
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum PlanModeState {
    Inactive,
    Pending,
    Active,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum MemoryFlushTrigger {
    SlashCommand,
    Interval,
    PreCompaction,
    UserRequested,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum MediaType {
    Image,
    Video,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum PagerCommandSource {
    Builtin,
    NonBuiltin,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum InstallKind {
    Git,
    Local,
}

impl InstallKind {
    /// Stable snake_case label matching the serde representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Local => "local",
        }
    }
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum PluginSource {
    LocalPath,
    Git,
}

// ─────────────────────────────────────────────────────────────────────────────
// Auth
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct Login {
    pub auth_method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

/// The login-method picker was shown. `trigger` is "startup", "logout", or
/// "mid_session".
#[derive(Serialize)]
pub struct LoginPickerShown {
    pub trigger: String,
}

/// A login method was chosen from the picker. `method` is "xai" or "api_key";
/// `mode` is "device", "loopback", or "api_key".
#[derive(Serialize)]
pub struct LoginMethodChosen {
    pub method: String,
    pub mode: String,
}

/// A login flow completed successfully. `method` is "xai" or "api_key";
/// `mode` is the resolved auth mode; `mid_session` is true for `/login`/401
/// re-auth (as opposed to the startup/logout flow).
#[derive(Serialize)]
pub struct LoginCompleted {
    pub method: String,
    pub mode: String,
    pub duration_ms: u64,
    pub mid_session: bool,
}

/// A login flow failed. `error` is the raw error message from the auth flow.
#[derive(Serialize)]
pub struct LoginFailed {
    pub method: String,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub duration_ms: u64,
}

/// The user backed out of the login funnel. `stage` is "picker",
/// "api_key_entry", "loopback_paste", "device_wait", "api_key_wait", or
/// "command_wait"; `via` is "esc" or "quit".
#[derive(Serialize)]
pub struct LoginAbandoned {
    pub stage: String,
    pub via: String,
}

/// Result of persisting a user-provided API key.
#[derive(Serialize)]
pub struct ApiKeySaveResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Plan Mode
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct PlanModeToggled {
    pub enabled: bool,
    pub trigger: PlanModeTrigger,
    pub turn_in_flight: bool,
    pub was_previously_active: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// Contextual tips
// ─────────────────────────────────────────────────────────────────────────────

/// One contextual-hint impression or acceptance: per tip, how often it is
/// shown vs. acted on (the `action` property drives the Mixpanel funnel).
#[derive(Serialize)]
pub struct ContextualTip {
    pub tip: ContextualTipKind,
    pub action: ContextualTipAction,
}

// ─────────────────────────────────────────────────────────────────────────────
// Prompt suggestions (tab autocomplete ghost text)
// ─────────────────────────────────────────────────────────────────────────────

/// One predicted-next-prompt ghost impression or outcome. `shown` →
/// `accepted` conversion is the acceptance rate of the tab-autocomplete
/// feature; `dismissed` counts explicit Esc dismissals (a shown suggestion
/// with neither outcome was implicitly ignored). No suggestion text is
/// logged — only content-free metadata.
#[derive(Serialize)]
pub struct PromptSuggestion {
    pub action: PromptSuggestionAction,
    /// Length of the full suggestion in characters (content-free size
    /// signal: are long or short suggestions likelier to be accepted?).
    pub chars: usize,
    /// Number of whitespace-separated words in the suggestion.
    pub words: usize,
}

// ─────────────────────────────────────────────────────────────────────────────
// Permission Mode
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct YoloToggled {
    pub enabled: bool,
    pub previous_state: bool,
    pub trigger: YoloTrigger,
}

// ─────────────────────────────────────────────────────────────────────────────
// Slash Commands
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct SlashCommandUsed {
    pub command: String,
    pub args_provided: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// Permissions
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct PermissionPrompted {
    pub tool_name: String,
    pub access_kind: AccessKind,
    pub permission_mode: PermissionMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_type: Option<String>,
}

#[derive(Serialize)]
pub struct PermissionDecisionPayload {
    pub tool_name: String,
    pub access_kind: AccessKind,
    pub decision: PermissionOutcome,
    pub wait_ms: u64,
    pub permission_mode: PermissionMode,
    /// Decision provenance (`config`/`user_reject`/`user_abort`/…), from
    /// shell's `permission_decision_source`. Additive analytics-visible field, added
    /// for the external `tool_decision` event (design ‡ footnote).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_type: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Auto-Compact
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct AutoCompactFired {
    pub tokens_before: u64,
    pub percentage: u8,
}

// ─────────────────────────────────────────────────────────────────────────────
// Compaction
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct CompactionTriggered {
    pub trigger: CompactionTrigger,
    pub tokens_used: u64,
    pub context_window: u64,
    pub percentage: u8,
    pub model_id: String,
    pub user_context_provided: bool,
    pub compaction_id: String,
}

#[derive(Serialize)]
pub struct CompactionCompleted {
    pub duration_ms: u64,
    pub tokens_before: u64,
    pub tokens_after: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    pub compaction_id: String,
}

/// Emits paired `compaction_triggered` + `compaction_completed` events with
/// a shared `compaction_id`. Guarantees both events fire and correlate.
pub struct CompactionScope {
    pub compaction_id: String,
    pub tokens_before: u64,
    pub model_id: String,
    start: std::time::Instant,
}

impl CompactionScope {
    pub fn begin(
        trigger: CompactionTrigger,
        tokens_used: u64,
        context_window: u64,
        model_id: String,
        user_context_provided: bool,
    ) -> Self {
        let compaction_id = uuid::Uuid::new_v4().to_string();
        let percentage = xai_token_estimation::usage_percentage_u8(tokens_used, context_window);
        crate::session_ctx::log_event(CompactionTriggered {
            trigger,
            tokens_used,
            context_window,
            percentage,
            model_id: model_id.clone(),
            user_context_provided,
            compaction_id: compaction_id.clone(),
        });
        Self {
            compaction_id,
            tokens_before: tokens_used,
            model_id,
            start: std::time::Instant::now(),
        }
    }

    pub fn complete(self, tokens_after: u64) {
        crate::session_ctx::log_event(CompactionCompleted {
            duration_ms: self.start.elapsed().as_millis() as u64,
            tokens_before: self.tokens_before,
            tokens_after,
            model_id: Some(self.model_id),
            compaction_id: self.compaction_id,
        });
    }
}

/// Auto-compaction suppressed after a deterministic failure so the turn loop stops
/// re-firing a doomed compaction. Fires once per transition into the suppressed
/// state; `reason` is a fixed classification: `credit_block | size | auth | schema | other`.
#[derive(Serialize)]
pub struct AutoCompactSuppressed {
    pub reason: &'static str,
    pub estimated_tokens: u64,
    pub context_window: u64,
}

#[derive(Serialize)]
pub struct CompactionRetryDegraded {
    pub trigger: CompactionTrigger,
    pub reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_stage: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_stage: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_chars: Option<u64>,
    pub attempt: u32,
    pub context_window: u64,
    pub compaction_id: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Subagents
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct SubagentLaunched {
    pub subagent_id: String,
    pub parent_session_id: String,
    pub subagent_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    pub fork_context: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_from: Option<String>,
    pub isolated_worktree: bool,
    pub mcp_inherited_count: u32,
    pub mcp_owned_count: u32,
    pub skills_inherited_count: u32,
}

#[derive(Serialize)]
pub struct SubagentCompleted {
    pub subagent_id: String,
    pub parent_session_id: String,
    pub outcome: Outcome,
    pub duration_ms: u64,
    pub tool_calls: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_used: Option<u64>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Model Switching
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct ModelSwitched {
    pub session_id: String,
    pub previous_model_id: String,
    pub new_model_id: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_agent_type: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Plugins
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct PluginAdded {
    pub source: PluginSource,
    pub success: bool,
}

#[derive(Serialize)]
pub struct PluginRemoved {
    pub success: bool,
}

#[derive(Serialize)]
pub struct PluginInstalled {
    pub install_kind: InstallKind,
    pub trust: bool,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_category: Option<String>,
}

#[derive(Serialize)]
pub struct PluginUninstalled {
    pub confirmed: bool,
    pub success: bool,
}

#[derive(Serialize)]
pub struct PluginReloaded {
    pub success: bool,
}

#[derive(Serialize)]
pub struct PluginUsed {
    pub plugin_id: String,
    pub plugin_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hook_event: Option<String>,
    pub success: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// Plugin CTA (inline marketplace "Connect" upsell)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct PluginCtaImpression {
    pub plugin_name: String,
}

#[derive(Serialize)]
pub struct PluginCtaConnectClicked {
    pub plugin_name: String,
    pub is_retry: bool,
}

#[derive(Serialize)]
pub struct PluginCtaDismissed {
    pub plugin_name: String,
}

#[derive(Serialize)]
pub struct PluginCtaInstalled {
    pub plugin_name: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_category: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Extensions modal
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionsModalTrigger {
    SlashCommand,
    KeyboardShortcut,
    CommandPalette,
    AuthHandoff,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionsInputMethod {
    Keyboard,
    Mouse,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionsModalTab {
    Hooks,
    Plugins,
    Marketplace,
    Skills,
    McpServers,
}

#[derive(Serialize)]
pub struct ExtensionsModalOpened {
    pub trigger: ExtensionsModalTrigger,
    pub tab: ExtensionsModalTab,
}

#[derive(Serialize)]
pub struct ExtensionsModalAction {
    pub tab: ExtensionsModalTab,
    pub action: String,
    pub input_method: ExtensionsInputMethod,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Hooks
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct HookAdded {
    pub success: bool,
}

#[derive(Serialize)]
pub struct HookRemoved {
    pub success: bool,
}

#[derive(Serialize)]
pub struct HookTrusted {
    pub success: bool,
}

#[derive(Serialize)]
pub struct HookExecuted {
    pub hook_name: String,
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    pub duration_ms: u64,
    pub outcome: HookOutcome,
}

#[derive(Serialize)]
pub struct HookBlocked {
    pub hook_name: String,
}

/// Per-callback outcome of a `PreToolUse` gate. A deny returns early, so callbacks
/// still pending at that point are not logged.
#[derive(Serialize)]
pub struct ClientHookGate {
    pub callback_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    pub outcome: ClientHookGateOutcome,
    pub duration_ms: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// Skills
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct SkillAdded {
    pub added_count: u32,
    pub total_skills: u32,
    pub success: bool,
}

#[derive(Serialize)]
pub struct SkillRemoved {
    pub success: bool,
}

#[derive(Serialize)]
pub struct SkillDispatched {
    pub skill_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_source: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// MCP
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct McpServerConnected {
    pub server_name: String,
    pub tool_count: u32,
    pub transport: McpTransport,
    pub duration_ms: u64,
}

#[derive(Serialize)]
pub struct McpServerFailed {
    pub server_name: String,
    pub error_type: McpErrorType,
    pub duration_ms: u64,
    pub timeout_sec: u64,
}

#[derive(Serialize)]
pub struct McpInitCompleted {
    pub total_duration_ms: u64,
    pub server_count: u32,
    pub servers_succeeded: u32,
    pub servers_failed: u32,
    pub servers_auth_required: u32,
    pub total_tools_registered: u32,
    pub strategy: McpStrategy,
    pub is_reinit: bool,
}

#[derive(Serialize)]
pub struct McpToolCalled {
    pub server_name: String,
    pub tool_name: String,
    pub qualified_name: String,
    pub success: bool,
    pub duration_ms: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// Session Lifecycle
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct SessionHarness {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_identifier: Option<String>,
    pub model_id: String,
    pub agent_name: String,
    pub permission_mode: PermissionMode,
    pub mcp_server_names: Vec<String>,
    pub plugin_names: Vec<String>,
    pub skill_names: Vec<String>,
    pub lsp_server_names: Vec<String>,
    pub hook_names: Vec<String>,
    pub agents_md_dir_names: Vec<String>,
    pub memory_enabled: bool,
    /// Whether the session cwd is inside a git repo (same value `SessionNew`
    /// carries). Additive analytics-visible field, added for the external
    /// `session_start` event (design ‡ footnote).
    pub is_git_repo: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_update: Option<bool>,
}

#[derive(Serialize)]
pub struct SessionLoad {
    pub session_id: String,
    pub compaction_count: u64,
    pub turn_count: u64,
    pub tool_call_count: u64,
    pub plan_mode_state: PlanModeState,
    pub permission_mode: PermissionMode,
    pub model_id: String,
    pub restored_from_disk: bool,
}

#[derive(Serialize)]
pub struct SessionNew {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_identifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
    pub is_git_repo: bool,
    pub permission_mode: PermissionMode,
}

#[derive(Serialize)]
pub struct PromptSubmitted {
    pub prompt_length: usize,
    pub model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_identifier: Option<String>,
    /// Pager screen mode from the prompt request `_meta.screenMode`
    /// (`fullscreen` | `inline` | `minimal` | `headless`). `None` for
    /// non-pager clients and synthetic prompts (goal summaries, drains,
    /// interjections).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_mode: Option<String>,
    /// Raw prompt text for the external stream's `OTEL_LOG_USER_PROMPTS`
    /// gate **only**. `#[serde(skip)]`: never serialized to product events/Mixpanel;
    /// dropped at external emit time unless the gate is on (then capped at
    /// 60 KB and secret-scrubbed).
    #[serde(skip)]
    pub prompt_text: Option<String>,
}

#[derive(Serialize)]
pub struct UserFeedback {
    pub session_id: String,
    pub has_feedback_text: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating_value: Option<i32>,
    pub is_solicited: bool,
}

#[derive(Serialize)]
pub struct RolloutSurvey {
    pub session_id: String,
    pub preferences: Vec<String>,
    pub has_feedback: bool,
}

/// PR created via the session (bash `gh pr create` or MCP create_pull_request).
/// Counts only — PR url/number stay in the turn_result.json signals, not here.
#[derive(Serialize)]
pub struct PrCreated {
    pub source: PrCreationSource,
    /// Whether the session recorded a `git commit` before the create
    /// (end-to-end attribution vs unknown work start).
    pub had_commit_in_session: bool,
}

/// PR merged via the session bash tool (`gh pr merge`).
#[derive(Serialize)]
pub struct PrMerged {}

#[derive(Serialize)]
pub struct MultiAgentFollowup {
    pub preferred_agent_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_agent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_agent_model_id: Option<String>,
    pub other_agents: Vec<AgentInfo>,
    pub total_agents: usize,
}

#[derive(Serialize)]
pub struct MultiAgentApply {
    pub applied_agent_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_agent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_agent_model_id: Option<String>,
    pub discarded_agents: Vec<AgentInfo>,
    pub total_agents: usize,
}

#[derive(Serialize)]
pub struct MultiAgentDiscard {
    pub discarded_agents: Vec<AgentInfo>,
    pub total_agents_discarded: usize,
}

#[derive(Serialize)]
pub struct AgentInfo {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
}

#[derive(Serialize)]
pub struct RepoChanges {
    pub commit_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staged_files_changed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staged_insertions: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staged_deletions: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unstaged_files_changed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unstaged_insertions: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unstaged_deletions: Option<u64>,
    pub untracked_file_count: usize,
    pub untracked_total_bytes: u64,
    pub is_detached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collection_id: Option<String>,
}

#[derive(Serialize)]
pub struct NonGitDecisionEvent {
    pub decision: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
}

// ---------------------------------------------------------------------------
// Prompt Latency (every turn)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct PromptLatency {
    pub turn_index: u32,
    pub total_ms: u64,
    pub mcp_wait_ms: u64,
    pub tool_collection_ms: u64,
    pub model_call_ms: u64,
    pub pre_model_ms: u64,
    pub mcp_server_count: u32,
    pub mcp_tools_registered: u32,
    pub mcp_strategy: McpStrategy,
    pub model_id: String,
}

// ---------------------------------------------------------------------------
// Turn Lifecycle
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct TurnCompleted {
    pub outcome: Outcome,
    pub duration_ms: u64,
    pub tool_call_count: u32,
    pub model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancellation_category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_category: Option<String>,
}

/// Model issued a shell tool call whose command is `true` (keepalive thrash signal).
#[derive(Serialize)]
pub struct ShellTrueNoop {
    pub tool_name: String,
}

/// Harness hard-stopped a turn after identical tool thrash (silent EndTurn).
#[derive(Serialize)]
pub struct ActionStationarityStop {
    pub true_noop: bool,
    pub run_len: u32,
    pub tool_name: String,
}

// ---------------------------------------------------------------------------
// Tool Calls
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct ToolCallCompleted {
    pub tool_name: String,
    pub outcome: xai_file_utils::events::types::ToolOutcome,
    pub duration_ms: u64,
    /// Primary file path of the call, for the external stream only
    /// (`#[serde(skip)]`: never serialized to product events/Mixpanel). Always reduced to
    /// `file_extension`; the full path rides the `OTEL_LOG_TOOL_DETAILS` gate.
    #[serde(skip)]
    pub file_path: Option<String>,
    /// Tool parameters for the external stream's `OTEL_LOG_TOOL_DETAILS`
    /// gate **only** (`#[serde(skip)]`; reduced to 4 KB / depth 2 / 20 items
    /// at emit time).
    #[serde(skip)]
    pub parameters: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Model Response
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct ModelResponseReceived {
    pub model_id: String,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_prompt_tokens: Option<u32>,
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct MemoryFlushed {
    pub trigger: MemoryFlushTrigger,
    pub success: bool,
    pub duration_ms: u64,
    pub response_length: usize,
}

// ---------------------------------------------------------------------------
// Media Generation
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct MediaGenerated {
    pub media_type: MediaType,
    pub success: bool,
    pub prompt_length: usize,
}

// ---------------------------------------------------------------------------
// Session End
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct SessionEnded {
    pub duration_secs: u64,
    pub turn_count: u64,
    pub tool_call_count: u64,
    pub compaction_count: u64,
    pub model_id: String,
}

// ---------------------------------------------------------------------------
// Pager events (called from xai-grok-pager via log_event)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct PagerSlashCommand {
    pub command_name: String,
    pub source: PagerCommandSource,
}

#[derive(Serialize)]
pub struct PlanSubmit {
    pub action: String,
}

/// Which option the user chose in the project-directory picker (shown on the
/// first prompt when Grok Build is launched from a non-project directory).
#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProjectPickerOutcome {
    RecentProject,
    CustomPath,
    CurrentDir,
    DontAskAgain,
    Dismissed,
}

impl ProjectPickerOutcome {
    pub fn picked_project(self) -> bool {
        matches!(self, Self::RecentProject | Self::CustomPath)
    }
}

/// User resolved the project-directory picker. `picked_project` is the headline
/// signal: did they actually choose a project directory?
#[derive(Serialize)]
pub struct ProjectPickerSelected {
    pub outcome: ProjectPickerOutcome,
    pub picked_project: bool,
    pub project_dir_options: usize,
}

// ---------------------------------------------------------------------------
// SuperGrok upsell
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SuperGrokUpsell {
    WelcomeScreen,
    RateLimitError,
    /// Free-usage-exhausted paywall modal (free-tier 429 with the
    /// `subscription:free-usage-exhausted` well-known error code).
    FreeUsagePaywall,
    /// Upsell modal shown when a tier-restricted slash command
    /// (`/usage`, `/imagine`, …) is invoked on the free / X Basic tiers.
    RestrictedCommand,
}

#[derive(Serialize)]
pub struct SuperGrokUpsellShown {
    pub source: SuperGrokUpsell,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,
}

#[derive(Serialize)]
pub struct SuperGrokUpsellClicked {
    pub source: SuperGrokUpsell,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,
}

/// Which surface a promo announcement's upgrade CTA was activated from.
/// Modeled on [`SuperGrokUpsell`]; lets the funnel attribute the click to the
/// welcome hero vs the in-session header vs the banner vs the dashboard, and
/// distinguish keyboard (`Ctrl+O`) activations from pointer/OSC 8 ones.
/// Ord/Eq exist for the pager's per-(announcement, surface) impression latch.
#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum AnnouncementCtaSurface {
    Banner,
    Welcome,
    Header,
    Dashboard,
    Keyboard,
}

/// A promo announcement's CTA button was painted on a surface — the
/// impression half of the per-surface CTR funnel with
/// [`AnnouncementCtaClicked`]. Emitted once per (announcement, surface) per
/// pager process (cleared on logout); never emitted for `Keyboard` (a
/// click-only surface).
#[derive(Serialize)]
pub struct AnnouncementCtaShown {
    /// Announcement `id` from the server push (`None` for id-less items).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Which surface painted the button.
    pub source: AnnouncementCtaSurface,
}

/// User activated a promo announcement's CTA button (the `[label]` open).
#[derive(Serialize)]
pub struct AnnouncementCtaClicked {
    /// Announcement `id` from the server push (`None` for id-less items).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Which surface the activation came from (per-surface conversion signal).
    pub source: AnnouncementCtaSurface,
}

/// Flat snapshot of the terminal environment for telemetry.
///
/// Shared across pager events so terminal fields are typed once.
/// Constructed by the pager's `TerminalContext::telemetry_snapshot()`.
#[derive(Clone, Debug, Serialize)]
pub struct TerminalTelemetry {
    pub brand: String,
    pub multiplexer: String,
    pub is_ssh: bool,
    pub is_byobu: bool,
    pub term_var: String,
    pub tmux_version: String,
    pub xtversion: String,
    pub host_os: String,
    pub display_server: String,
    pub modifier_cmd_fate: String,
    pub modifier_opt_fate: String,
    pub enter_modifier_fate: String,
    pub hyperlink_osc8: String,
    pub hyperlink_skip_reason: String,
    pub clipboard_route: String,
    pub clipboard_native_tool: String,
    /// Wayland data-control protocol availability: "yes" | "no" | "n/a"
    /// (n/a off Wayland).
    pub clipboard_data_control: String,
}

/// One-shot OS primary-display refresh probe + auto-cadence decision at process start.
#[derive(Serialize)]
pub struct DisplayRefreshProbe {
    #[serde(flatten)]
    pub terminal: TerminalTelemetry,
    /// `ok` | `skipped` | `error`
    pub outcome: String,
    /// Refresh rate as `i64` so OTLP/analytics keep a numeric field.
    pub hz: Option<i64>,
    /// Backend token, e.g. `macos_core_graphics`.
    pub source: String,
    /// Empty when ok; else stable skip/error reason (`ssh`, `wsl`, …).
    pub skip_reason: String,
    /// Wall ms as `i64` so OTLP/analytics keep a numeric field (u64 serializes as string).
    pub duration_ms: i64,
    pub auto_cadence_enabled: bool,
    /// True when derived auto ms is used on at least one motion clock.
    pub auto_cadence_applied: bool,
    pub effective_min_draw_ms: i64,
    pub effective_scroll_cadence_ms: i64,
    /// `flag_off` | `disabled` | `probe_skip` | `hz_out_of_range` | `env_override` | `applied`.
    pub auto_cadence_reason: String,
}

/// Emitted once per system-clipboard attachment read during paste
/// (Ctrl/Cmd+V). Diagnoses silent image-paste failures, e.g. Wayland-only
/// sessions where the X11 CLIPBOARD probe comes back empty.
#[derive(Serialize)]
pub struct ClipboardImagePaste {
    #[serde(flatten)]
    pub terminal: TerminalTelemetry,
    /// Which read ran: "attachments" (file URLs + image) or "image".
    pub probe: String,
    /// "image" | "file_urls" | "empty" | "error".
    pub outcome: String,
    /// MIME type when outcome == "image", else "".
    pub image_mime: String,
    /// Wall-clock duration of the clipboard read in milliseconds.
    pub duration_ms: u64,
}

/// Emitted when Ctrl/Cmd+V (paste key) is handled but the **host** process
/// clipboard has no pasteable text/image/file URLs. Diagnoses silent no-ops
/// on remote/ETX sessions. Does not change paste behavior.
#[derive(Serialize)]
pub struct PasteKeyEmptyHostClipboard {
    #[serde(flatten)]
    pub terminal: TerminalTelemetry,
    /// Call site: "agent" | "prompt_widget" | "dashboard" | "peek" | "picker".
    pub surface: String,
}

/// Emitted once per user-visible text copy (`copy_text` / TUI yank, etc.).
///
/// Captures per-leg write outcomes so we can diagnose "copy doesn't work"
/// reports (e.g. Wayland + xclip probe; did wl-copy actually succeed?) without
/// relying on toast text alone.
#[derive(Serialize)]
pub struct ClipboardCopy {
    #[serde(flatten)]
    pub terminal: TerminalTelemetry,
    /// Call site; currently always `copy_text`.
    pub source: &'static str,
    /// Payload size only (no content).
    pub text_len: u64,
    /// Route policy (enabled legs), independent of which legs succeeded.
    pub route_native: bool,
    pub route_tmux: bool,
    pub route_osc52: bool,
    /// `ClipboardRoute` Display, e.g. `native+osc52`.
    pub route_label: String,
    /// CLI tools actually invoked, `+`-joined (e.g. `wl-copy+xclip`); empty if none.
    pub cli_tools_tried: String,
    /// CLI tools that returned Ok, `+`-joined; empty if none succeeded.
    /// On Wayland, wl-copy is read-back-verified only when `data_control` is
    /// false; with `data_control && arboard_ok` its exit-0 is credited
    /// unverified (the arboard write is authoritative) — condition wl-copy
    /// success rates on `data_control`.
    pub cli_ok_tools: String,
    pub cli_ok: bool,
    pub arboard_ok: bool,
    /// The Wayland data-control protocol was available for this write (the
    /// environment probe — NOT proof the arboard write landed; a focus-free
    /// authoritative write additionally requires `arboard_ok`). Always false
    /// off-Wayland.
    pub data_control: bool,
    pub tmux_ok: bool,
    pub osc52_ok: bool,
    /// Evidence classification: `confirmed` | `unverified` | `failed`.
    pub delivery: &'static str,
    /// An explicit `grok wrap` OSC 52 sink was active.
    pub osc52_sink: bool,
    /// The process was inside a container without a display server.
    pub container_no_display: bool,
    /// Historical boolean projection: true unless `delivery == failed`.
    pub reported_success: bool,
    /// Exact UX toast branch selected by the environment policy.
    pub toast_kind: &'static str,
    pub duration_ms: u64,
}

/// Emitted when backspace/delete is pressed but produces no text change
/// on a non-empty prompt. Used to diagnose the "backspace lock" bug.
#[derive(Serialize)]
pub struct BackspaceNoEffect {
    #[serde(flatten)]
    pub terminal: TerminalTelemetry,
    pub key_code: String,
    pub key_modifiers: String,
    pub key_kind: String,
    pub cursor_pos: usize,
    pub text_len: usize,
    pub has_selection: bool,
}

/// Emitted each time a terminal notification is actually sent (not filtered
/// by condition or event kind). Used for protocol distribution analysis.
#[derive(Serialize)]
pub struct NotificationEmitted {
    pub protocol: &'static str,
    pub event_kind: &'static str,
    pub was_focused: bool,
}

#[derive(Serialize)]
pub struct DashboardOpened {
    pub agents: usize,
    pub subagents: usize,
    pub leader_mode: bool,
}

#[derive(Serialize)]
pub struct DashboardClosed {
    pub agents: usize,
}

#[derive(Serialize)]
pub struct DashboardAgentAttached {
    pub kind: &'static str,
}

#[derive(Serialize)]
pub struct DashboardAgentLaunched {
    pub source: &'static str,
}

// ---------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------

/// Emitted when a user's turn fails due to rate limiting (all retries
/// exhausted). Key conversion-funnel signal: rate limit → upsell → subscribe.
#[derive(Serialize)]
pub struct RateLimitHit {
    pub model_id: String,
    /// Number of retry attempts before giving up.
    pub attempts: u32,
}

/// Model-API failure at the turn level (non-rate-limit). Category/class only
/// — no message text (external `api_error` event; also a product event).
#[derive(Serialize)]
pub struct ApiError {
    /// Fixed classification (`auth`, `server_error`, `timeout`, …).
    pub error_category: String,
    pub model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// Internal (our-code) error class for the external `internal_error` event.
/// Error class only — no message, no location (user decision, RQ5).
#[derive(Serialize)]
pub struct InternalError {
    pub error_type: String,
}

// ---------------------------------------------------------------------------
// External-OTEL stream meta-events (product-events only — adoption visibility;
// never exported externally)
// ---------------------------------------------------------------------------

/// Emitted once per process (post-auth) when the external OTEL stream is
/// configured. Endpoint reduced to `scheme://host[:port]` — we measure
/// adoption without learning collector details.
#[derive(Serialize)]
pub struct ExternalOtelConfigured {
    pub metrics_exporter: String,
    pub logs_exporter: String,
    pub protocol: String,
    pub logs_endpoint_origin: String,
    pub metrics_endpoint_origin: String,
    pub prompts_gate: bool,
    pub details_gate: bool,
    /// Startup source of the master switch: `env` | `config`.
    pub source: String,
}

/// Remote (fleet) policy applied to the external stream mid-run.
#[derive(Serialize)]
pub struct ExternalOtelRemotePolicyApplied {
    /// `force_disable` | `gates_locked`.
    pub action: String,
}

/// Export-health counters for the external stream, emitted on the internal
/// pipeline at shutdown (never externally — avoid feedback loops).
#[derive(Serialize)]
pub struct ExternalOtelExportHealth {
    pub records_dropped: u64,
    pub metric_exports_dropped: u64,
    pub export_failures: u64,
    pub export_successes: u64,
}

// ---------------------------------------------------------------------------
// Credit limit
// ---------------------------------------------------------------------------

/// 403 "run out of credits" — billing exhaustion (not request throttling).
#[derive(Serialize)]
pub struct CreditLimitHit {
    pub model_id: String,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum CreditLimitUpsellSurface {
    /// Q&A modal with "Upgrade tier" + "Pay as you go" options (non-max-tier).
    QuestionModal,
    /// Inline scrollback card with PAYG link (max-tier / Heavy users).
    InlineCard,
}

/// Credit-limit upsell displayed to the user.
#[derive(Serialize)]
pub struct CreditLimitUpsellShown {
    pub surface: CreditLimitUpsellSurface,
    pub max_tier: bool,
    pub pay_as_you_go: bool,
    /// User is on unified usage billing (buy-credits wording). When false,
    /// legacy on-demand / PAYG wording was used.
    #[serde(default)]
    pub unified_billing: bool,
}

#[derive(Debug, Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum CreditLimitChoice {
    UpgradeTier,
    /// Covers both "Pay as you go" (enable) and "Increase limit" (raise cap).
    PayAsYouGo,
    /// Unified-billing / credits-pool users: purchase prepaid credits.
    PurchaseCredits,
}

/// User clicked an option in the credit-limit upsell.
#[derive(Serialize)]
pub struct CreditLimitUpsellClicked {
    pub surface: CreditLimitUpsellSurface,
    pub choice: CreditLimitChoice,
}

// ---------------------------------------------------------------------------
// Subscription conversion
// ---------------------------------------------------------------------------

/// Emitted when a previously access-gated user re-authenticates and the gate
/// is lifted — i.e. they subscribed (externally on grok.com) and came back.
/// This is the actual conversion signal for SuperGrok Heavy subscriptions
/// attributed to Grok Build: the user saw the gate in Grok Build, went and
/// paid, then returned with access.
#[derive(Serialize)]
pub struct SubscriptionActivated {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,
    /// Whether the subscribe CTA was shown in this session before the gate
    /// was lifted (`access_gate_shown_logged`). When `true`, the conversion
    /// is strongly attributable to Grok Build's upsell surface.
    pub upsell_shown_this_session: bool,
}

/// Why auth recovery could not refresh the credential, forcing the user to
/// manually re-authenticate. Mapped from shell's `AuthError`; only terminal
/// failures map — transient ones don't emit (recovery retries).
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ManualAuthReason {
    /// IdP rejected the refresh token (`invalid_grant`); a re-login is required.
    RefreshTokenRejected,
    /// Token type has no refresh authority (API key / legacy / OIDC sans refresh token).
    NoRefreshAuthority,
    RecoveryExhausted,
    TokenExpiredNoRefresh,
    /// Recovered session violated the `force_login_team_uuid` pin.
    WrongTeam,
}

/// User-facing surface where the manual re-auth was triggered. Background
/// recoveries (storage/telemetry uploads) do not emit this event.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ManualAuthSurface {
    /// A chat/inference turn (the yellow `ReAuthRequired` banner).
    Turn,
    /// The relay / leader connection handshake.
    Relay,
}

/// The kind of bearer that was rejected. Mirrors shell's `TokenType` as a
/// stable wire enum (don't serialize the shell `Debug` repr).
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum AuthTokenKind {
    OidcSession,
    ExternalBinary,
    LegacySession,
    ApiKey,
    None,
}

/// KPI: a user-facing 401 recovery (`Turn`/`Relay`) terminally failed, forcing a
/// manual re-login. Product-events only (no external export).
///
/// Alerting contract: the event lands under the Shell-origin name
/// `grok-shell-manual_auth` (the `manual_auth` binding gets the `grok-shell-`
/// prefix at emit). Count `distinct(principal)`, never raw events — the debounce
/// is a single slot per process (repeats on the most-recent dead credential
/// collapse; alternating credentials can re-emit), and `trigger` is whichever
/// surface fired first, not a reliable per-surface split. `principal` is absent for
/// unattributed lockouts (all collapse into one NULL bucket). API-key sessions
/// are excluded (a 401 there means rotate the key, not `/login`).
// `Debug`/`Clone`/`PartialEq` let shell tests assert the emitted event by value
// (a downstream crate's `cfg(test)` can't turn on `cfg_attr(test, ...)` here).
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct ManualAuth {
    pub reason: ManualAuthReason,
    pub trigger: ManualAuthSurface,
    pub token_kind: AuthTokenKind,
    /// `user_id` of the locked-out account, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Event name bindings
// ─────────────────────────────────────────────────────────────────────────────

telemetry_event!(ManualAuth, "manual_auth");

telemetry_event!(Login, "login", external = crate::external::schema::map_auth);
telemetry_event!(LoginPickerShown, "login_picker_shown");
telemetry_event!(LoginMethodChosen, "login_method_chosen");
telemetry_event!(LoginCompleted, "login_completed");
telemetry_event!(LoginFailed, "login_failed");
telemetry_event!(LoginAbandoned, "login_abandoned");
telemetry_event!(ApiKeySaveResult, "api_key_save_result");
telemetry_event!(
    PlanModeToggled,
    "plan_mode_toggled",
    external = crate::external::schema::map_plan_mode_toggled
);
telemetry_event!(
    ContextualTip,
    "contextual_tip",
    external = crate::external::schema::map_contextual_tip
);
telemetry_event!(PromptSuggestion, "prompt_suggestion");
telemetry_event!(
    YoloToggled,
    "yolo_toggled",
    external = crate::external::schema::map_yolo_toggled
);
telemetry_event!(SlashCommandUsed, "slash_command_used");
telemetry_event!(PermissionPrompted, "permission_prompted");
telemetry_event!(
    PermissionDecisionPayload,
    "permission_decision",
    external = crate::external::schema::map_tool_decision
);
telemetry_event!(AutoCompactFired, "auto_compact_fired");
telemetry_event!(CompactionTriggered, "compaction_triggered");
telemetry_event!(
    CompactionCompleted,
    "compaction_completed",
    external = crate::external::schema::map_compaction
);
telemetry_event!(AutoCompactSuppressed, "auto_compact_suppressed");
telemetry_event!(CompactionRetryDegraded, "compaction_retry_degraded");
telemetry_event!(
    SubagentLaunched,
    "subagent_launched",
    external = crate::external::schema::map_subagent_launched
);
telemetry_event!(
    SubagentCompleted,
    "subagent_completed",
    external = crate::external::schema::map_subagent_completed
);
telemetry_event!(
    ModelSwitched,
    "model_switched",
    external = crate::external::schema::map_model_switched
);
telemetry_event!(PluginAdded, "plugin_added");
telemetry_event!(PluginRemoved, "plugin_removed");
telemetry_event!(
    PluginInstalled,
    "plugin_installed",
    external = crate::external::schema::map_plugin_installed
);
telemetry_event!(PluginUninstalled, "plugin_uninstalled");
telemetry_event!(PluginReloaded, "plugin_reloaded");
telemetry_event!(
    PluginUsed,
    "plugin_used",
    external = crate::external::schema::map_plugin_used
);
telemetry_event!(PluginCtaImpression, "plugin_cta_impression");
telemetry_event!(PluginCtaConnectClicked, "plugin_cta_connect_clicked");
telemetry_event!(PluginCtaDismissed, "plugin_cta_dismissed");
telemetry_event!(PluginCtaInstalled, "plugin_cta_installed");
telemetry_event!(ExtensionsModalOpened, "extensions_modal_opened");
telemetry_event!(ExtensionsModalAction, "extensions_modal_action");
telemetry_event!(HookAdded, "hook_added");
telemetry_event!(HookRemoved, "hook_removed");
telemetry_event!(HookTrusted, "hook_trusted");
telemetry_event!(HookExecuted, "hook_executed");
telemetry_event!(HookBlocked, "hook_blocked");
telemetry_event!(ClientHookGate, "client_hook_gate");
telemetry_event!(SkillAdded, "skill_added");
telemetry_event!(SkillRemoved, "skill_removed");
telemetry_event!(
    SkillDispatched,
    "skill_dispatched",
    external = crate::external::schema::map_skill_activated
);
telemetry_event!(
    McpServerConnected,
    "mcp_server_connected",
    external = crate::external::schema::map_mcp_server_connected
);
telemetry_event!(
    McpServerFailed,
    "mcp_server_failed",
    external = crate::external::schema::map_mcp_server_failed
);
telemetry_event!(McpInitCompleted, "mcp_init_completed");
telemetry_event!(McpToolCalled, "mcp_tool_called");
telemetry_event!(
    SessionHarness,
    "session_harness",
    external = crate::external::schema::map_session_start
);
telemetry_event!(SessionLoad, "session_load");
telemetry_event!(
    SessionNew,
    "session_new",
    external = crate::external::schema::map_session_new
);
telemetry_event!(
    PromptSubmitted,
    "prompt_submitted",
    external = crate::external::schema::map_user_prompt
);
telemetry_event!(UserFeedback, "user_feedback");
telemetry_event!(RolloutSurvey, "rollout_survey");
telemetry_event!(PrCreated, "pr_created");
telemetry_event!(PrMerged, "pr_merged");
telemetry_event!(MultiAgentFollowup, "multi_agent_followup");
telemetry_event!(MultiAgentApply, "multi_agent_apply");
telemetry_event!(MultiAgentDiscard, "multi_agent_discard");
telemetry_event!(RepoChanges, "repo_changes");
telemetry_event!(NonGitDecisionEvent, "non_git_decision");
telemetry_event!(PromptLatency, "prompt_latency");
telemetry_event!(
    TurnCompleted,
    "turn_completed",
    external = crate::external::schema::map_turn_completed
);
telemetry_event!(ShellTrueNoop, "shell_true_noop");
telemetry_event!(ActionStationarityStop, "action_stationarity_stop");
telemetry_event!(
    ToolCallCompleted,
    "tool_call_completed",
    external = crate::external::schema::map_tool_result
);
telemetry_event!(
    ModelResponseReceived,
    "model_response_received",
    external = crate::external::schema::map_api_request
);
telemetry_event!(MemoryFlushed, "memory_flushed");
telemetry_event!(MediaGenerated, "media_generated");
telemetry_event!(
    SessionEnded,
    "session_ended",
    external = crate::external::schema::map_session_end
);
telemetry_event!(PagerSlashCommand, "pager_slash_command");
telemetry_event!(PlanSubmit, "plan_submit");
telemetry_event!(ProjectPickerSelected, "project_picker_selected");
telemetry_event!(SuperGrokUpsellShown, "supergrok_upsell_shown");
telemetry_event!(SuperGrokUpsellClicked, "supergrok_upsell_clicked");
telemetry_event!(AnnouncementCtaShown, "announcement_cta_shown");
telemetry_event!(AnnouncementCtaClicked, "announcement_cta_clicked");
telemetry_event!(TerminalTelemetry, "terminal_context");
telemetry_event!(DisplayRefreshProbe, "display_refresh_probe");
telemetry_event!(BackspaceNoEffect, "backspace_no_effect");
telemetry_event!(ClipboardImagePaste, "clipboard_image_paste");
telemetry_event!(PasteKeyEmptyHostClipboard, "paste_key_empty_host_clipboard");
telemetry_event!(ClipboardCopy, "clipboard_copy");
telemetry_event!(NotificationEmitted, "notification_emitted");
telemetry_event!(DashboardOpened, "dashboard_opened");
telemetry_event!(DashboardClosed, "dashboard_closed");
telemetry_event!(DashboardAgentAttached, "dashboard_agent_attached");
telemetry_event!(DashboardAgentLaunched, "dashboard_agent_launched");
telemetry_event!(
    RateLimitHit,
    "rate_limit_hit",
    external = crate::external::schema::map_rate_limit_hit
);
telemetry_event!(CreditLimitHit, "credit_limit_hit");
telemetry_event!(CreditLimitUpsellShown, "credit_limit_upsell_shown");
telemetry_event!(CreditLimitUpsellClicked, "credit_limit_upsell_clicked");
telemetry_event!(SubscriptionActivated, "subscription_activated");
telemetry_event!(
    ApiError,
    "api_error",
    external = crate::external::schema::map_api_error
);
telemetry_event!(
    InternalError,
    "internal_error",
    external = crate::external::schema::map_internal_error
);
telemetry_event!(ExternalOtelConfigured, "external_otel_configured");
telemetry_event!(
    ExternalOtelRemotePolicyApplied,
    "external_otel_remote_policy_applied"
);
telemetry_event!(ExternalOtelExportHealth, "external_otel_export_health");

// Session lifecycle (structs in session_metrics)
telemetry_event!(crate::session_metrics::SessionStarted, "session_started");
telemetry_event!(crate::session_metrics::Turn, "turn");
telemetry_event!(
    crate::session_metrics::TurnCompletedLifecycle,
    "turn_completed_lifecycle"
);
telemetry_event!(
    crate::session_metrics::DoomLoopRecovery,
    "doom_loop_recovery"
);
telemetry_event!(
    crate::session_metrics::TraceUploadAttempted,
    "trace_upload_attempted"
);
telemetry_event!(
    crate::session_metrics::TraceUploadSucceeded,
    "trace_upload_succeeded"
);
telemetry_event!(
    crate::session_metrics::TraceUploadSkipped,
    "trace_upload_skipped"
);
telemetry_event!(
    crate::session_metrics::TraceUploadFailed,
    "trace_upload_failed"
);

// Memory subsystem (structs in memory_telemetry)
telemetry_event!(
    crate::memory_telemetry::MemorySessionInit,
    "memory_session_init"
);
telemetry_event!(crate::memory_telemetry::MemorySearch, "memory_search");
telemetry_event!(
    crate::memory_telemetry::MemorySearchEmpty,
    "memory_search_empty"
);
telemetry_event!(
    crate::memory_telemetry::MemoryFlushStart,
    "memory_flush_start"
);
telemetry_event!(
    crate::memory_telemetry::MemoryFlushComplete,
    "memory_flush_complete"
);
telemetry_event!(crate::memory_telemetry::MemoryInjection, "memory_injection");
telemetry_event!(crate::memory_telemetry::MemoryReindex, "memory_reindex");
telemetry_event!(
    crate::memory_telemetry::MemoryWatcherSync,
    "memory_watcher_sync"
);
telemetry_event!(
    crate::memory_telemetry::MemorySessionSummary,
    "memory_session_summary"
);

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal_telemetry_fixture() -> TerminalTelemetry {
        TerminalTelemetry {
            brand: "Unknown".into(),
            multiplexer: "none".into(),
            is_ssh: true,
            is_byobu: false,
            term_var: "xterm-256color".into(),
            tmux_version: "".into(),
            xtversion: "".into(),
            host_os: "linux".into(),
            display_server: "unknown".into(),
            modifier_cmd_fate: "unknown".into(),
            modifier_opt_fate: "unknown".into(),
            enter_modifier_fate: "unknown".into(),
            hyperlink_osc8: "unknown".into(),
            hyperlink_skip_reason: "none".into(),
            clipboard_route: "native+osc52".into(),
            clipboard_native_tool: "arboard".into(),
            clipboard_data_control: "n/a".into(),
        }
    }

    #[test]
    fn clipboard_copy_serialization_preserves_boolean_and_adds_delivery_evidence() {
        for delivery in ["confirmed", "unverified", "failed"] {
            let value = serde_json::to_value(ClipboardCopy {
                terminal: terminal_telemetry_fixture(),
                source: "copy_text",
                text_len: 12,
                route_native: true,
                route_tmux: false,
                route_osc52: true,
                route_label: "native+osc52".into(),
                cli_tools_tried: String::new(),
                cli_ok_tools: String::new(),
                cli_ok: false,
                arboard_ok: false,
                data_control: false,
                tmux_ok: false,
                osc52_ok: true,
                delivery,
                osc52_sink: false,
                container_no_display: false,
                reported_success: delivery != "failed",
                toast_kind: "unverified_osc_remote",
                duration_ms: 1,
            })
            .unwrap();
            assert_eq!(value["delivery"], serde_json::json!(delivery));
            assert_eq!(
                value["reported_success"],
                serde_json::Value::Bool(delivery != "failed")
            );
            assert_eq!(value["osc52_sink"], serde_json::json!(false));
            assert_eq!(value["container_no_display"], serde_json::json!(false));
        }
    }

    #[test]
    fn manual_auth_name_and_shape() {
        assert_eq!(ManualAuth::NAME, "manual_auth");

        let with_principal = serde_json::to_value(ManualAuth {
            reason: ManualAuthReason::RefreshTokenRejected,
            trigger: ManualAuthSurface::Turn,
            token_kind: AuthTokenKind::OidcSession,
            principal: Some("user-1".into()),
        })
        .unwrap();
        assert_eq!(
            with_principal,
            serde_json::json!({
                "reason": "refresh_token_rejected",
                "trigger": "turn",
                "token_kind": "oidc_session",
                "principal": "user-1",
            })
        );

        // `principal` is omitted (not null) when unknown. `LegacySession` is a
        // reachable fixture (API-key sessions never emit this event).
        let no_principal = serde_json::to_value(ManualAuth {
            reason: ManualAuthReason::NoRefreshAuthority,
            trigger: ManualAuthSurface::Relay,
            token_kind: AuthTokenKind::LegacySession,
            principal: None,
        })
        .unwrap();
        assert!(!no_principal.as_object().unwrap().contains_key("principal"));
    }

    #[test]
    fn project_picker_selected_name_and_shape() {
        assert_eq!(ProjectPickerSelected::NAME, "project_picker_selected");

        let picked = serde_json::to_value(ProjectPickerSelected {
            outcome: ProjectPickerOutcome::RecentProject,
            picked_project: ProjectPickerOutcome::RecentProject.picked_project(),
            project_dir_options: 3,
        })
        .unwrap();
        assert_eq!(
            picked,
            serde_json::json!({
                "outcome": "recent_project",
                "picked_project": true,
                "project_dir_options": 3,
            })
        );

        let dismissed = serde_json::to_value(ProjectPickerSelected {
            outcome: ProjectPickerOutcome::Dismissed,
            picked_project: ProjectPickerOutcome::Dismissed.picked_project(),
            project_dir_options: 1,
        })
        .unwrap();
        assert_eq!(
            dismissed,
            serde_json::json!({
                "outcome": "dismissed",
                "picked_project": false,
                "project_dir_options": 1,
            })
        );
    }

    #[test]
    fn project_picker_outcome_picked_project_mapping() {
        assert!(ProjectPickerOutcome::RecentProject.picked_project());
        assert!(ProjectPickerOutcome::CustomPath.picked_project());
        assert!(!ProjectPickerOutcome::CurrentDir.picked_project());
        assert!(!ProjectPickerOutcome::DontAskAgain.picked_project());
        assert!(!ProjectPickerOutcome::Dismissed.picked_project());
    }

    #[test]
    fn project_picker_outcome_wire_strings() {
        let s = |o: ProjectPickerOutcome| serde_json::to_value(o).unwrap();
        assert_eq!(
            s(ProjectPickerOutcome::RecentProject),
            serde_json::json!("recent_project")
        );
        assert_eq!(
            s(ProjectPickerOutcome::CustomPath),
            serde_json::json!("custom_path")
        );
        assert_eq!(
            s(ProjectPickerOutcome::CurrentDir),
            serde_json::json!("current_dir")
        );
        assert_eq!(
            s(ProjectPickerOutcome::DontAskAgain),
            serde_json::json!("dont_ask_again")
        );
        assert_eq!(
            s(ProjectPickerOutcome::Dismissed),
            serde_json::json!("dismissed")
        );
    }

    #[test]
    fn plugin_cta_event_names() {
        assert_eq!(PluginCtaImpression::NAME, "plugin_cta_impression");
        assert_eq!(PluginCtaConnectClicked::NAME, "plugin_cta_connect_clicked");
        assert_eq!(PluginCtaDismissed::NAME, "plugin_cta_dismissed");
        assert_eq!(PluginCtaInstalled::NAME, "plugin_cta_installed");
    }

    #[test]
    fn announcement_cta_event_names() {
        assert_eq!(AnnouncementCtaShown::NAME, "announcement_cta_shown");
        assert_eq!(AnnouncementCtaClicked::NAME, "announcement_cta_clicked");
    }

    #[test]
    fn compaction_retry_degraded_name_and_shape() {
        assert_eq!(CompactionRetryDegraded::NAME, "compaction_retry_degraded");

        let degenerate = serde_json::to_value(CompactionRetryDegraded {
            trigger: CompactionTrigger::Auto,
            reason: "degenerate_summary",
            from_stage: None,
            to_stage: None,
            summary_chars: Some(130),
            attempt: 1,
            context_window: 128_000,
            compaction_id: "cid-1".into(),
        })
        .unwrap();
        assert_eq!(
            degenerate,
            serde_json::json!({
                "trigger": "auto",
                "reason": "degenerate_summary",
                "summary_chars": 130,
                "attempt": 1,
                "context_window": 128_000,
                "compaction_id": "cid-1",
            })
        );

        let overflow = serde_json::to_value(CompactionRetryDegraded {
            trigger: CompactionTrigger::Manual,
            reason: "input_overflow",
            from_stage: Some("verbatim"),
            to_stage: Some("verbatim_fitted"),
            summary_chars: None,
            attempt: 2,
            context_window: 128_000,
            compaction_id: "cid-2".into(),
        })
        .unwrap();
        assert_eq!(
            overflow,
            serde_json::json!({
                "trigger": "manual",
                "reason": "input_overflow",
                "from_stage": "verbatim",
                "to_stage": "verbatim_fitted",
                "attempt": 2,
                "context_window": 128_000,
                "compaction_id": "cid-2",
            })
        );
    }

    #[test]
    fn plugin_cta_impression_serializes_plugin_name() {
        let v = serde_json::to_value(PluginCtaImpression {
            plugin_name: "figma".into(),
        })
        .unwrap();
        assert_eq!(v, serde_json::json!({ "plugin_name": "figma" }));
    }

    #[test]
    fn plugin_cta_connect_clicked_serializes_is_retry() {
        let fresh = serde_json::to_value(PluginCtaConnectClicked {
            plugin_name: "figma".into(),
            is_retry: false,
        })
        .unwrap();
        assert_eq!(
            fresh,
            serde_json::json!({ "plugin_name": "figma", "is_retry": false })
        );
        let retry = serde_json::to_value(PluginCtaConnectClicked {
            plugin_name: "figma".into(),
            is_retry: true,
        })
        .unwrap();
        assert_eq!(
            retry,
            serde_json::json!({ "plugin_name": "figma", "is_retry": true })
        );
    }

    #[test]
    fn plugin_cta_installed_omits_error_category_when_none() {
        let v = serde_json::to_value(PluginCtaInstalled {
            plugin_name: "figma".into(),
            success: true,
            error_category: None,
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "plugin_name": "figma", "success": true })
        );
    }

    #[test]
    fn login_funnel_event_names() {
        assert_eq!(LoginPickerShown::NAME, "login_picker_shown");
        assert_eq!(LoginMethodChosen::NAME, "login_method_chosen");
        assert_eq!(LoginCompleted::NAME, "login_completed");
        assert_eq!(LoginFailed::NAME, "login_failed");
        assert_eq!(LoginAbandoned::NAME, "login_abandoned");
        assert_eq!(ApiKeySaveResult::NAME, "api_key_save_result");
    }

    #[test]
    fn login_completed_serializes_all_fields() {
        let v = serde_json::to_value(LoginCompleted {
            method: "xai".into(),
            mode: "device".into(),
            duration_ms: 1234,
            mid_session: false,
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "method": "xai",
                "mode": "device",
                "duration_ms": 1234,
                "mid_session": false,
            })
        );
    }

    #[test]
    fn api_key_save_result_omits_error_when_ok() {
        let ok = serde_json::to_value(ApiKeySaveResult {
            ok: true,
            error: None,
        })
        .unwrap();
        assert_eq!(ok, serde_json::json!({ "ok": true }));
        let err = serde_json::to_value(ApiKeySaveResult {
            ok: false,
            error: Some("failed to write key".into()),
        })
        .unwrap();
        assert_eq!(
            err,
            serde_json::json!({ "ok": false, "error": "failed to write key" })
        );
    }

    #[test]
    fn plugin_cta_installed_includes_error_category_when_some() {
        let v = serde_json::to_value(PluginCtaInstalled {
            plugin_name: "figma".into(),
            success: false,
            error_category: Some("not_found".into()),
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "plugin_name": "figma",
                "success": false,
                "error_category": "not_found",
            })
        );
    }
}
