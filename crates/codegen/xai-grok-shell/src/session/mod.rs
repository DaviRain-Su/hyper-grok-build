pub mod acp_types;
pub mod announcement_state;
pub mod commands;
pub mod compaction_config;
pub mod handle;
pub mod memory_state;
pub mod merge;
pub mod notifications;
pub mod pending_interaction;
pub mod prompt_queue;
pub mod two_pass;
pub use self::acp_session::*;
pub use self::acp_types::*;
pub use self::commands::*;
pub use self::fork::{ForkSessionRequest, ForkSessionResponse, fork_session};
pub use self::handle::*;
pub use self::persistence::{
    LocalFeedbackEntry, UserFeedbackEntry, find_local_child_for_remote, resolve_local_session,
    resolve_local_session_any_cwd, session_exists_by_id, session_exists_for_cwd,
};
pub use self::result::{Empty, ExtMethodResult};
pub use self::share::{ShareSessionRequest, ShareSessionResponse};
pub use prod_mc_cli_chat_proxy_types::feedback_types::{
    ClientType, FeedbackTerminalInfo, RatingType,
};
pub use xai_fsnotify::{FsConfig, FsEvent, FsEventKind, FsEventSource, FsNotifyError, GitMetaKind};
/// `false` twin: this template is not compiled into this build, so no
/// template matches. Keeps ungated call sites compiling in both
/// configurations.
pub(crate) fn is_cursor_user_template(
    _template: &xai_grok_agent::prompt::user_message::UserMessageTemplate,
) -> bool {
    false
}
/// `false` twin of [`is_cursor_system_template`]; see [`is_cursor_user_template`].
pub(crate) fn is_cursor_system_template(
    _template: &xai_grok_agent::prompt::context::TemplateOverride,
) -> bool {
    false
}
/// Pull the `ContentBlock::Image`s out of a block list — the single spelling
/// of "only Image blocks ride structurally" (interject parse + queue-interject
/// harvest).
pub(crate) fn image_blocks(
    blocks: impl IntoIterator<Item = agent_client_protocol::ContentBlock>,
) -> Vec<agent_client_protocol::ImageContent> {
    blocks
        .into_iter()
        .filter_map(|block| match block {
            agent_client_protocol::ContentBlock::Image(img) => Some(img),
            _ => None,
        })
        .collect()
}
/// Describes who originated a prompt: the user, or the shell's auto-wake
/// system reacting to a completed background task / subagent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptOrigin {
    /// A normal user-initiated prompt.
    User,
    /// Auto-wake prompt injected when a background terminal task completed.
    TaskCompleted {
        /// The background task ID (without the `task-completed-` prefix).
        task_id: String,
    },
    /// Auto-wake prompt injected when a background subagent completed.
    SubagentCompleted {
        /// The subagent ID (without the `subagent-completed-` prefix).
        subagent_id: String,
    },
    WorkflowCompleted {
        completion_id: String,
    },
    /// Server-initiated prompt from the idle-gated notification drain
    /// (`maybe_drain_notifications`). Batches one or more monitor-event
    /// or bash-task-completed notifications into a single turn while the
    /// user is idle.
    NotificationDrain,
    /// Orchestrator-initiated summary turn. The goal orchestrator injects a
    /// system reminder into context and then triggers a model turn so the
    /// model can print a visible progress update.
    GoalSummary,
    /// Verification-stage nudge injected after the verification stage
    /// achieved — keep working" system-reminder body alongside the
    /// path to the persisted details file. The variant name retains
    /// the `Classifier` prefix for wire stability.
    GoalClassifierNudge,
    /// Scheduled task (`/loop`) prompt fired by the scheduler via the pager.
    SchedulerFired,
    /// Turn injected after a resumed plan-approval decision: the
    /// shell re-parked `exit_plan_mode` on resume, the user approved/revised,
    /// and the shell injects the follow-up turn. Synthetic so the user never
    /// typed it — kept out of prompt history — but it still runs a real turn.
    PlanResume,
}
impl PromptOrigin {
    /// Parse a prompt_id string into a `PromptOrigin`.
    pub fn from_prompt_id(prompt_id: &str) -> Self {
        if let Some(task_id) = prompt_id.strip_prefix("task-completed-") {
            Self::TaskCompleted {
                task_id: task_id.to_string(),
            }
        } else if let Some(subagent_id) = prompt_id.strip_prefix("subagent-completed-") {
            Self::SubagentCompleted {
                subagent_id: subagent_id.to_string(),
            }
        } else if let Some(completion_id) = prompt_id.strip_prefix("workflow-completed-") {
            Self::WorkflowCompleted {
                completion_id: completion_id.to_string(),
            }
        } else if prompt_id.starts_with("notifications-") {
            Self::NotificationDrain
        } else if prompt_id.starts_with("goal-summary-") {
            Self::GoalSummary
        } else if prompt_id.starts_with("goal-classifier-nudge-") {
            Self::GoalClassifierNudge
        } else if prompt_id.starts_with("scheduler-fired-") {
            Self::SchedulerFired
        } else if prompt_id.starts_with("plan-resume-") {
            Self::PlanResume
        } else {
            Self::User
        }
    }
    /// Returns `true` for auto-wake (synthetic) prompts.
    pub fn is_synthetic(&self) -> bool {
        !matches!(self, Self::User)
    }
    /// Whether a `UserMessageChunk` echo for this origin must stay out of
    /// client scrollback (live and on resume). Model-only / side-channel
    /// content — UI already surfaces it via task pane, monitor gutter, etc.
    ///
    /// Cron (`SchedulerFired`) and plan-resume follow-ups still render;
    /// real user turns always render.
    pub fn hide_user_echo_from_scrollback(&self) -> bool {
        match self {
            Self::User | Self::SchedulerFired | Self::PlanResume => false,
            Self::TaskCompleted { .. }
            | Self::SubagentCompleted { .. }
            | Self::WorkflowCompleted { .. }
            | Self::NotificationDrain
            | Self::GoalSummary
            | Self::GoalClassifierNudge => true,
        }
    }
    pub fn completion_id(&self) -> Option<&str> {
        match self {
            Self::TaskCompleted { task_id } => Some(task_id),
            Self::SubagentCompleted { subagent_id } => Some(subagent_id),
            Self::WorkflowCompleted { completion_id } => Some(completion_id),
            Self::User
            | Self::NotificationDrain
            | Self::GoalSummary
            | Self::GoalClassifierNudge
            | Self::SchedulerFired
            | Self::PlanResume => None,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::PromptOrigin;
    #[test]
    fn from_prompt_id_user() {
        assert_eq!(
            PromptOrigin::from_prompt_id("my-prompt"),
            PromptOrigin::User
        );
        assert!(!PromptOrigin::from_prompt_id("my-prompt").is_synthetic());
    }
    #[test]
    fn from_prompt_id_task_completed() {
        let origin = PromptOrigin::from_prompt_id("task-completed-abc-123");
        assert_eq!(
            origin,
            PromptOrigin::TaskCompleted {
                task_id: "abc-123".into()
            }
        );
        assert!(origin.is_synthetic());
        assert_eq!(origin.completion_id(), Some("abc-123"));
    }
    #[test]
    fn from_prompt_id_subagent_completed() {
        let origin = PromptOrigin::from_prompt_id("subagent-completed-xyz-789");
        assert_eq!(
            origin,
            PromptOrigin::SubagentCompleted {
                subagent_id: "xyz-789".into()
            }
        );
        assert!(origin.is_synthetic());
        assert_eq!(origin.completion_id(), Some("xyz-789"));
    }
    #[test]
    fn from_prompt_id_notification_drain() {
        let origin =
            PromptOrigin::from_prompt_id("notifications-019e0000-0000-7000-8000-0000000000aa");
        assert_eq!(origin, PromptOrigin::NotificationDrain);
        assert!(origin.is_synthetic());
        assert_eq!(origin.completion_id(), None);
    }
    #[test]
    fn goal_summary_origin_from_prompt_id() {
        let origin = PromptOrigin::from_prompt_id("goal-summary-019e2d3e");
        assert!(matches!(origin, PromptOrigin::GoalSummary));
        assert!(origin.is_synthetic());
        assert_eq!(origin.completion_id(), None);
    }
    #[test]
    fn goal_classifier_nudge_origin_from_prompt_id() {
        let origin = PromptOrigin::from_prompt_id("goal-classifier-nudge-019e2d3e");
        assert!(matches!(origin, PromptOrigin::GoalClassifierNudge));
        assert!(origin.is_synthetic());
        assert_eq!(origin.completion_id(), None);
    }
    #[test]
    fn goal_classifier_nudge_origin_round_trips_through_from_prompt_id() {
        let prompt_id = format!("goal-classifier-nudge-{}", uuid::Uuid::now_v7());
        let origin = PromptOrigin::from_prompt_id(&prompt_id);
        assert!(matches!(origin, PromptOrigin::GoalClassifierNudge));
        assert!(origin.is_synthetic());
    }
    #[test]
    fn scheduler_fired_origin_from_prompt_id() {
        let origin = PromptOrigin::from_prompt_id("scheduler-fired-019e51a3-abcd-1234");
        assert!(matches!(origin, PromptOrigin::SchedulerFired));
        assert!(origin.is_synthetic());
        assert_eq!(origin.completion_id(), None);
    }
    #[test]
    fn plan_resume_origin_from_prompt_id() {
        let origin = PromptOrigin::from_prompt_id("plan-resume-1730000000000");
        assert!(matches!(origin, PromptOrigin::PlanResume));
        assert!(origin.is_synthetic());
        assert_eq!(origin.completion_id(), None);
    }
    #[test]
    fn notification_drain_is_server_initiated() {
        let prompt_id = "notifications-019e0000-0000-7000-8000-0000000000aa";
        assert!(PromptOrigin::from_prompt_id(prompt_id).is_synthetic());
    }
    #[test]
    fn hide_user_echo_from_scrollback_by_origin() {
        assert!(!PromptOrigin::User.hide_user_echo_from_scrollback());
        assert!(
            !PromptOrigin::from_prompt_id("scheduler-fired-abc").hide_user_echo_from_scrollback()
        );
        assert!(!PromptOrigin::from_prompt_id("plan-resume-1").hide_user_echo_from_scrollback());
        assert!(PromptOrigin::from_prompt_id("task-completed-t1").hide_user_echo_from_scrollback());
        assert!(
            PromptOrigin::from_prompt_id("subagent-completed-s1").hide_user_echo_from_scrollback()
        );
        assert!(
            PromptOrigin::from_prompt_id("notifications-uuid").hide_user_echo_from_scrollback()
        );
        assert!(
            PromptOrigin::from_prompt_id("workflow-completed-wf-1-9")
                .hide_user_echo_from_scrollback()
        );
        assert!(PromptOrigin::from_prompt_id("goal-summary-1").hide_user_echo_from_scrollback());
        assert!(
            PromptOrigin::from_prompt_id("goal-classifier-nudge-1")
                .hide_user_echo_from_scrollback()
        );
    }
}
/// Client-requested fs notification mode (was xai_fsnotify::FsNotifyMode).
/// Determines whether the session sends an initial file index to the client
/// or just streams raw file events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum ClientFsMode {
    #[default]
    Events,
    Index,
}
/// Client-side fs notification config: fs source settings + mode.
#[derive(Debug, Clone, Default)]
pub struct ClientFsConfig {
    pub fs: FsConfig,
    pub mode: ClientFsMode,
}
/// Share session request/response types
pub mod share {
    /// Request to share a session via URL
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    pub struct ShareSessionRequest {
        pub session_id: String,
    }
    /// Response containing the shareable URL
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    pub struct ShareSessionResponse {
        pub share_url: String,
    }
}
/// Proxy config for the session registry client.
/// Shared between `acp_session` (slash commands) and `persistence` (title generation).
#[derive(Clone)]
pub(crate) struct RegistryConfig {
    pub base_url: String,
    pub user_token: String,
    pub deployment_key: Option<String>,
    pub alpha_test_key: Option<String>,
}
pub mod acp_conversion;
pub mod acp_mcp;
pub(crate) mod acp_session;
pub(crate) mod agent_rebuild;
pub mod chat_persistence;
pub(crate) mod events;
pub mod export;
pub mod feedback;
pub mod feedback_manager;
pub mod file_system;
pub mod fork;
pub(crate) mod fs_watch;
pub(crate) mod goal_classifier;
pub(crate) mod goal_evaluator;
pub(crate) mod goal_next_step;
pub(crate) mod goal_orchestrator;
pub(crate) mod goal_planner;
pub(crate) mod goal_role_tools;
pub(crate) mod goal_stop_detector;
pub(crate) mod goal_strategist;
pub(crate) mod goal_summarizer;
pub mod goal_tracker;
pub mod helpers;
pub(crate) mod image_describe;
pub(crate) mod image_normalize;
pub mod inference_metrics;
pub use xai_grok_shared::session::info;
pub mod managed_mcp;
pub(crate) mod mcp_descriptors;
pub mod mcp_dispatcher;
#[cfg(test)]
mod mcp_dispatcher_e2e_tests;
pub mod mcp_restart;
pub mod mcp_servers;
pub mod memory;
pub(crate) mod normalize_cache;
pub mod persistence;
pub use xai_grok_shared::placeholder_images;
pub mod plan_mode;
pub mod prompt_history;
pub mod prompt_parser;
pub(crate) mod prompt_timing;
pub(crate) mod replay_events;
pub mod repo_changes;
#[path = "restore_stub.rs"]
pub mod restore;
pub mod result;
pub mod signals;
pub(crate) mod slash_commands;
pub mod storage;
pub(crate) mod streaming_capture;
pub(crate) mod summary;
pub(crate) mod telemetry;
pub mod tool_index;
pub(crate) mod turn_completion;
pub mod unified_list;
mod user_message;
pub(crate) mod wire_tags;
pub(crate) mod workflow;
pub mod worktree;
pub mod worktree_pool;
