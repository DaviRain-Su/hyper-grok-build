use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::tool::{ToolKind, ToolNamespace};

use super::task::types::SubagentDepthCounter;

pub use xai_grok_tools_api::slash_commands::WORKFLOW_TOOL_NAME;

/// Short name of a tool id (`GrokBuild:workflow` → `workflow`).
pub fn workflow_tool_short_name(id: &str) -> &str {
    id.rsplit(':').next().unwrap_or(id)
}

/// True when the id is `workflow` or `*:workflow`.
pub fn is_workflow_tool_id(id: &str) -> bool {
    workflow_tool_short_name(id) == WORKFLOW_TOOL_NAME
}

/// Child sessions must not receive the workflow tool. Match by kind **or**
/// short id so kindless `ToolConfig::from_id` / tools-server entries drop too.
pub fn is_workflow_tool(kind: Option<ToolKind>, id: &str) -> bool {
    kind == Some(ToolKind::Workflow) || is_workflow_tool_id(id)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowSource {
    Name {
        #[schemars(
            description = "Name of a registered workflow (built-in, or discovered from the project `.grok/workflows/` or user `~/.grok/workflows/`)."
        )]
        name: String,
    },
    Script {
        #[schemars(
            description = "Inline Rhai workflow script. It must start with a pure-literal `let meta = #{ name: ..., description: ... };` map. Before authoring, read the `create-workflow` skill's SKILL.md. Run the path-specific `validate_only` smoke check with representative args."
        )]
        script: String,
    },
    ScriptPath {
        #[schemars(description = "Path to a .rhai workflow script on disk.")]
        script_path: String,
    },
    Resume {
        #[schemars(
            description = "Resume a same-process paused run, continuing its original immutable source and args. A budget-limited run resumes only when `agent_budget` is passed with a higher cap. Process-restart interruptions are terminal."
        )]
        resume_from_run_id: String,
    },
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct WorkflowToolInput {
    #[schemars(
        description = "Exactly one workflow source. The `type` tag selects a registered name, inline script, script path, or same-process resume."
    )]
    pub source: WorkflowSource,

    #[serde(default)]
    #[schemars(
        range(min = 1, max = 1024),
        description = "Absolute cumulative cap on logical child-agent calls for this run. Every agent() and every parallel() item consumes one slot; schema retries do not. Defaults to 128 and may be set from 1 through 1,024. A panel that would exceed the remaining budget is rejected before any of its children launch."
    )]
    pub agent_budget: Option<u64>,

    #[serde(default)]
    #[schemars(
        description = "JSON value bound to the script's `args` global. Use an object for named arguments."
    )]
    pub args: Option<serde_json::Value>,

    #[serde(default)]
    #[schemars(
        description = "Run a path-specific smoke check without launching: validate metadata, compile the full script, and execute the single path selected by the supplied args and canned host results. It does not exercise every branch or prove live tools and agent outputs work."
    )]
    pub validate_only: bool,
}

#[derive(serde::Deserialize)]
struct WorkflowToolInputWire {
    #[serde(default)]
    source: Option<WorkflowSource>,
    #[serde(default)]
    agent_budget: Option<u64>,
    #[serde(default)]
    args: Option<serde_json::Value>,
    #[serde(default)]
    validate_only: bool,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    script: Option<String>,
    #[serde(default)]
    script_path: Option<String>,
    #[serde(default)]
    resume_from_run_id: Option<String>,
}

impl<'de> serde::Deserialize<'de> for WorkflowToolInput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;

        let mut wire = WorkflowToolInputWire::deserialize(deserializer)?;
        wire.name = nonblank(wire.name);
        wire.script = nonblank(wire.script);
        wire.script_path = nonblank(wire.script_path);
        wire.resume_from_run_id = nonblank(wire.resume_from_run_id);
        let legacy_sources = [
            wire.name.is_some(),
            wire.script.is_some(),
            wire.script_path.is_some(),
            wire.resume_from_run_id.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        if wire.source.is_some() && legacy_sources != 0 {
            return Err(D::Error::custom(
                "`source` cannot be combined with legacy `name`, `script`, `script_path`, or `resume_from_run_id` fields",
            ));
        }
        if legacy_sources > 1 {
            return Err(D::Error::custom(
                "workflow source fields are mutually exclusive; provide exactly one of `name`, `script`, `script_path`, or `resume_from_run_id`",
            ));
        }
        let source = wire
            .source
            .or_else(|| wire.name.map(|name| WorkflowSource::Name { name }))
            .or_else(|| wire.script.map(|script| WorkflowSource::Script { script }))
            .or_else(|| {
                wire.script_path
                    .map(|script_path| WorkflowSource::ScriptPath { script_path })
            })
            .or_else(|| {
                wire.resume_from_run_id
                    .map(|resume_from_run_id| WorkflowSource::Resume { resume_from_run_id })
            })
            .ok_or_else(|| {
                D::Error::custom(
                    "missing workflow source; provide `source` with exactly one of the `name`, `script`, `script_path`, or `resume` variants",
                )
            })?;
        Ok(Self {
            source,
            agent_budget: wire.agent_budget,
            args: wire.args,
            validate_only: wire.validate_only,
        })
    }
}

fn nonblank(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

impl WorkflowToolInput {
    pub const MAX_AGENT_BUDGET: u64 = 1_024;

    pub fn normalize(&mut self) {
        match &mut self.source {
            WorkflowSource::Name { name } => *name = name.trim().to_owned(),
            WorkflowSource::Script { script } => {
                if script.trim().is_empty() {
                    script.clear();
                }
            }
            WorkflowSource::ScriptPath { script_path } => {
                *script_path = script_path.trim().to_owned();
            }
            WorkflowSource::Resume { resume_from_run_id } => {
                *resume_from_run_id = resume_from_run_id.trim().to_owned();
            }
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if let Some(budget) = self.agent_budget {
            if budget == 0 {
                return Err("`agent_budget` must be a positive integer".into());
            }
            if budget > Self::MAX_AGENT_BUDGET {
                return Err(format!(
                    "`agent_budget` must be at most {} agents",
                    Self::MAX_AGENT_BUDGET
                ));
            }
        }
        let value = match &self.source {
            WorkflowSource::Name { name } => name,
            WorkflowSource::Script { script } => script,
            WorkflowSource::ScriptPath { script_path } => script_path,
            WorkflowSource::Resume { resume_from_run_id } => {
                if self.args.is_some() {
                    return Err(
                        "resume uses the original immutable source and arguments; do not provide `args`"
                            .into(),
                    );
                }
                if self.validate_only {
                    return Err("`validate_only` cannot be used when resuming a run".into());
                }
                resume_from_run_id
            }
        };
        if value.trim().is_empty() {
            return Err("workflow source value must not be blank".into());
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct WorkflowLaunchRequest {
    pub input: WorkflowToolInput,
}

#[derive(Debug)]
pub enum WorkflowLaunchAck {
    Started {
        run_id: String,
        task_id: String,
        name: String,
        script_path: Option<String>,
    },
    Validated {
        name: String,
        phases: usize,
        summary: String,
    },
    Rejected {
        code: &'static str,
        detail: String,
    },
}

pub type WorkflowLaunchEnvelope = (
    WorkflowLaunchRequest,
    tokio::sync::oneshot::Sender<WorkflowLaunchAck>,
);

pub struct WorkflowLaunchHandle(pub tokio::sync::mpsc::UnboundedSender<WorkflowLaunchEnvelope>);

impl std::fmt::Debug for WorkflowLaunchHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowLaunchHandle").finish()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct WorkflowToolOutput {
    pub run_id: String,
    #[schemars(
        description = "Alias of run_id; workflow runs are not background tasks — do not pass to task_output/wait_tasks. Completion notifies automatically."
    )]
    pub task_id: String,
    #[schemars(
        description = "The session-unique display handle for this run, such as review-changes or review-changes-2. Use it in user-facing status and /workflow management; keep run_id internal."
    )]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_path: Option<String>,
    pub message: String,
}

impl xai_tool_runtime::ToolOutput for WorkflowToolOutput {}

#[derive(Debug, Default)]
pub struct WorkflowTool;

impl crate::types::tool_metadata::ToolMetadata for WorkflowTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Workflow
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        r##"Launch a workflow: a Rhai script that orchestrates subagents as one background run. Provide exactly one `source`: a registered workflow `name`, an inline `script`, a `script_path`, or a same-process `resume`. Optionally pass `args` (bound to the script's `args`) and `agent_budget`, an absolute cap on cumulative child-agent calls: every agent() and parallel() item consumes one slot (schema retries do not); default 128. The host also caps live children per run (32 by default, host-configured) — larger parallel() panels are queued and still act as a barrier. The call returns immediately; progress appears in `/workflow runs`${%- if system_reminders_enabled %} and completion is reported automatically — do not poll or sleep-wait${%- endif %}.

Prefer a registered workflow when one fits; author a script for bounded fan-out over a known work list, staged research and verification, or several independent perspectives. Before writing or editing a script, read the `create-workflow` skill's SKILL.md. `validate_only: true` runs a path-specific smoke check (metadata, compile, one canned-host path) — not proof that every branch or live tool works.

A started run gets a session-unique display name (e.g. `review-changes`, `review-changes-2`) — the handle to show the user and use with `/workflow pause|resume|stop <name>`; keep run IDs internal. Each launch persists an editable `script_path`; edit it and launch as a new run to iterate. Use the `resume` source only for a same-process paused run (process restarts are terminal); it reuses the run's original immutable source and args, and a budget-limited run resumes only with a higher `agent_budget`. Save reusable scripts to `.grok/workflows/<name>.rhai`."##
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }

    fn is_read_only(&self) -> bool {
        false
    }
}

impl xai_tool_runtime::Tool for WorkflowTool {
    type Args = WorkflowToolInput;
    type Output = WorkflowToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(WORKFLOW_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            WORKFLOW_TOOL_NAME,
            crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: false,
            tool_scope: Some(xai_tool_protocol::ToolScope::Write),
            ..Default::default()
        }
    }

    #[tracing::instrument(name = "new_tool.workflow", skip_all)]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        mut input: WorkflowToolInput,
    ) -> Result<WorkflowToolOutput, xai_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        let resources = shared_resources(&ctx)?;

        input.normalize();

        if let Err(detail) = input.validate() {
            return Err(xai_tool_runtime::ToolError::custom(
                "workflow_invalid_input",
                detail,
            ));
        }

        let (depth, sender) = {
            let res = resources.lock().await;
            let depth = res.get::<SubagentDepthCounter>().map(|d| d.0).unwrap_or(0);
            let sender = res.get::<WorkflowLaunchHandle>().map(|h| h.0.clone());
            (depth, sender)
        };

        // Workflows stay top-level-only regardless of configurable subagent depth.
        if depth > 0 {
            return Err(xai_tool_runtime::ToolError::custom(
                "workflow_depth_exceeded",
                "Workflows can only be launched from a top-level session (subagents and \
                 workflow-spawned agents cannot start workflows)",
            ));
        }

        let sender = sender.ok_or_else(|| {
            xai_tool_runtime::ToolError::custom(
                "workflow_not_available",
                "Workflow launching is not available in this session (WorkflowLaunchHandle not \
                 registered)",
            )
        })?;

        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel::<WorkflowLaunchAck>();
        sender
            .send((WorkflowLaunchRequest { input }, ack_tx))
            .map_err(|_| {
                xai_tool_runtime::ToolError::custom(
                    "workflow_channel_closed",
                    "Workflow launch channel closed — the session may be shutting down",
                )
            })?;

        match ack_rx.await {
            Ok(WorkflowLaunchAck::Started {
                run_id,
                task_id,
                name,
                script_path,
            }) => Ok(WorkflowToolOutput {
                message: {
                    let iterate = script_path
                        .as_deref()
                        .map(|p| {
                            format!(
                                " The editable script projection is at {p}. Edit it and launch \
                                 that `script_path` as a new run to iterate; same-process pause \
                                 resume continues only this run's original immutable source."
                            )
                        })
                        .unwrap_or_default();
                    format!(
                        "Workflow '{name}' started in the background. Progress appears in \
                         /workflow runs and completion is reported automatically. '{name}' is \
                         the session-unique display handle for user-facing status and /workflow \
                         management; keep the structured run id internal.{iterate}"
                    )
                },
                run_id,
                task_id,
                name,
                script_path,
            }),
            Ok(WorkflowLaunchAck::Validated {
                name,
                phases,
                summary,
            }) => Ok(WorkflowToolOutput {
                message: format!(
                    "Smoke check passed for workflow '{name}' ({phases} declared phases; \
                     canned-host path {summary}). This did not launch the workflow and did not \
                     exercise every branch or live dependency. Offer a real run next."
                ),
                run_id: String::new(),
                task_id: String::new(),
                name,
                script_path: None,
            }),
            Ok(WorkflowLaunchAck::Rejected { code, detail }) => {
                Err(xai_tool_runtime::ToolError::custom(code, detail))
            }
            Err(_) => Err(xai_tool_runtime::ToolError::custom(
                "workflow_launch_no_ack",
                "The session dropped the launch channel before answering; the workflow may not \
                 have started.",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(value: serde_json::Value) -> Result<WorkflowToolInput, serde_json::Error> {
        serde_json::from_value(value)
    }

    #[test]
    fn model_schema_requires_one_tagged_source() {
        let schema = crate::registry::types::generate_schema::<WorkflowToolInput>();
        assert_eq!(schema["required"], serde_json::json!(["source"]));
        let source = &schema["properties"]["source"];
        assert_eq!(
            source["oneOf"].as_array().map(Vec::len),
            Some(4),
            "{source}"
        );
        assert!(schema["properties"].get("name").is_none());
        assert!(schema["properties"].get("script").is_none());
        assert!(schema["properties"].get("script_path").is_none());
        assert!(schema["properties"].get("resume_from_run_id").is_none());
        assert_eq!(
            schema["properties"]["agent_budget"]["default"],
            serde_json::Value::Null
        );
        assert_eq!(
            schema["properties"]["args"]["default"],
            serde_json::Value::Null
        );
        assert_eq!(schema["properties"]["validate_only"]["default"], false);
    }

    #[test]
    fn tagged_sources_serialize_and_deserialize() {
        let cases = [
            serde_json::json!({"source": {"type": "name", "name": "deep-research"}}),
            serde_json::json!({"source": {"type": "script", "script": "let meta = #{};"}}),
            serde_json::json!({"source": {"type": "script_path", "script_path": "flow.rhai"}}),
            serde_json::json!({"source": {"type": "resume", "resume_from_run_id": "wf_123"}}),
        ];
        for case in cases {
            let input = parse(case.clone()).unwrap();
            assert!(input.validate().is_ok(), "{case}");
            let serialized = serde_json::to_value(input).unwrap();
            assert_eq!(serialized["source"], case["source"]);
            assert_eq!(serialized["agent_budget"], serde_json::Value::Null);
            assert_eq!(serialized["args"], serde_json::Value::Null);
            assert_eq!(serialized["validate_only"], false);
        }
    }

    #[test]
    fn legacy_sources_remain_deserializable() {
        let cases = [
            serde_json::json!({"name": "deep-research"}),
            serde_json::json!({"script": "let meta = #{};"}),
            serde_json::json!({"script_path": "flow.rhai"}),
            serde_json::json!({"resume_from_run_id": "wf_123"}),
            serde_json::json!({
                "name": "deep-research",
                "script": " ",
                "script_path": "",
                "resume_from_run_id": "\t"
            }),
        ];
        for case in cases {
            assert!(parse(case).unwrap().validate().is_ok());
        }
    }

    #[test]
    fn missing_and_every_conflicting_legacy_source_combination_are_rejected() {
        assert!(parse(serde_json::json!({})).is_err());
        let fields = [
            ("name", "named"),
            ("script", "inline"),
            ("script_path", "flow.rhai"),
            ("resume_from_run_id", "wf_123"),
        ];
        for mask in 1_u8..(1 << fields.len()) {
            if mask.count_ones() < 2 {
                continue;
            }
            let mut object = serde_json::Map::new();
            for (index, (key, value)) in fields.iter().enumerate() {
                if mask & (1 << index) != 0 {
                    object.insert((*key).into(), serde_json::Value::String((*value).into()));
                }
            }
            assert!(parse(object.into()).is_err(), "mask {mask:04b}");
        }
    }

    #[test]
    fn tagged_source_cannot_be_combined_with_another_source() {
        assert!(
            parse(serde_json::json!({
                "source": {"type": "name", "name": "deep-research"},
                "script": "let meta = #{};"
            }))
            .is_err()
        );
        assert!(
            parse(serde_json::json!({
                "source": {
                    "type": "name",
                    "name": "deep-research",
                    "script": "let meta = #{};"
                }
            }))
            .is_err()
        );
    }

    #[test]
    fn runtime_options_remain_valid_and_resume_preserves_arguments() {
        let input = parse(serde_json::json!({
            "source": {"type": "script_path", "script_path": "flow.rhai"},
            "args": {"objective": "review", "files": ["a.rs"]},
            "agent_budget": 10,
            "validate_only": true
        }))
        .unwrap();
        assert!(input.validate().is_ok());
        assert_eq!(input.args.unwrap()["objective"], "review");

        let resume_with_args = parse(serde_json::json!({
            "source": {"type": "resume", "resume_from_run_id": "wf_123"},
            "args": {"changed": true}
        }))
        .unwrap();
        assert_eq!(
            resume_with_args.validate().unwrap_err(),
            "resume uses the original immutable source and arguments; do not provide `args`"
        );

        let legacy_resume_with_args = parse(serde_json::json!({
            "resume_from_run_id": "wf_123",
            "args": {"changed": true}
        }))
        .unwrap();
        assert!(legacy_resume_with_args.validate().is_err());
    }

    #[test]
    fn validation_rejects_blank_sources_and_invalid_budgets() {
        for source in [
            WorkflowSource::Name { name: " ".into() },
            WorkflowSource::Script { script: "".into() },
            WorkflowSource::ScriptPath {
                script_path: "\t".into(),
            },
            WorkflowSource::Resume {
                resume_from_run_id: "".into(),
            },
        ] {
            let input = WorkflowToolInput {
                source,
                agent_budget: None,
                args: None,
                validate_only: false,
            };
            assert!(input.validate().is_err());
        }

        for agent_budget in [0, WorkflowToolInput::MAX_AGENT_BUDGET + 1] {
            let input = WorkflowToolInput {
                source: WorkflowSource::Name {
                    name: "deep-research".into(),
                },
                agent_budget: Some(agent_budget),
                args: None,
                validate_only: false,
            };
            assert!(input.validate().is_err());
        }
    }

    #[test]
    fn workflow_id_matches_kind_or_short_name() {
        assert!(is_workflow_tool_id("workflow"));
        assert!(is_workflow_tool_id("GrokBuild:workflow"));
        assert!(!is_workflow_tool_id("web_search"));
        assert!(is_workflow_tool(Some(ToolKind::Workflow), "anything"));
        assert!(is_workflow_tool(None, "GrokBuild:workflow"));
        assert!(!is_workflow_tool(Some(ToolKind::Read), "read_file"));
    }
}
