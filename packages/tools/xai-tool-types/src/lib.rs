//! Canonical, extensible tool types.
mod ext;
mod schema_utils;
pub mod serde_lenient;
mod task;
mod types;

pub use ext::Extensions;
pub use schema_utils::parse_arguments_from_schema_lossy;
pub use serde_lenient::{
    deserialize_lenient_bool, deserialize_lenient_option_bool, lenient_bool_from_json,
};
pub use task::{
    BUILTIN_SUBAGENTS, BuiltinSubagent, EXPLORE_PROMPT, EXPLORE_SUBAGENT, GENERAL_PURPOSE_PROMPT,
    GENERAL_PURPOSE_SUBAGENT, KillTaskOutput, KillTaskResult, KillTaskToolInput,
    KillTaskToolNaming, MAX_MULTI_WAIT_IDS, MAX_SAFE_PATH_SEGMENT_LEN, MAX_WAIT_BLOCK_MS_DEFAULT,
    MAX_WAIT_MS_PLACEHOLDER, MultiTaskOutputResult, ORACLE_PROMPT, ORACLE_SUBAGENT, PLAN_PROMPT,
    PLAN_SUBAGENT, SubagentCapabilityMode, SubagentCompletedOutput, SubagentDescriptor,
    SubagentIsolationMode, SubagentReasoningEffort, SubagentToolNaming, TaskOutputOutput,
    TaskOutputResult, TaskOutputToolInput, TaskOutputToolNaming, TaskToolInput, TaskToolNaming,
    WaitMode, WaitTasksToolInput, WaitTasksToolNaming, XDOTCOM_PROMPT, XDOTCOM_SUBAGENT,
    build_kill_task_description, build_task_description, build_task_output_description,
    build_wait_tasks_description, builtin_subagent_by_name, default_subagent_type,
    format_resume_footer, format_subagent_completed, format_subagent_started_background,
    format_wait_cap_ms, is_not_sentinel, is_safe_path_segment, is_safe_task_id, max_wait_block_ms,
    resolve_task_ids, sanitize_optional_arg, sanitize_task_id, task_output_waits,
    task_output_waits_from_json,
};
pub use types::{
    ArgumentType, SchemaType, ToolArgument, ToolDescription, ValidationError, ValidationErrors,
};
