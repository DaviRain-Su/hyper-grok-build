pub mod command;
pub mod http;

use std::time::Duration;

use crate::config::HookSpec;
use crate::event::HookEventEnvelope;
use serde::Deserialize;

use crate::result::{HookDecision, HttpInfo, StopHookOutcome};

/// How a hook's output is interpreted, per the event's [`GateKind`]: `Observe`
/// ignores output, `Tool` parses the allow/deny vocabulary, `Stop` the stop
/// vocabulary.
pub use crate::event::GateKind;

pub struct RunContext<'a> {
    pub session_id: &'a str,
    pub workspace_root: &'a str,
    pub process_scope: Option<xai_grok_tools::util::ProcessScope>,
}

/// Result of running a single hook (any handler type).
#[derive(Debug)]
pub enum HookRunnerResult {
    Decision(HookDecision),
    Stop(StopHookOutcome),
    Success,
    /// Failed: the caller fails open.
    Failed(String),
}

/// JSON from `PreToolUse` gate hooks:
/// `{"decision": "allow" | "deny", "reason": "…"}`.
#[derive(Debug, Deserialize)]
pub(crate) struct GateHookJson {
    pub decision: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Interpret a [`GateHookJson`] as a [`HookDecision`]. An unknown decision value
/// is an error so typos surface instead of failing open.
pub(crate) fn gate_json_to_decision(
    json: GateHookJson,
    hook_name: &str,
) -> Result<HookDecision, String> {
    match json.decision.as_str() {
        "deny" => Ok(HookDecision::Deny {
            reason: json
                .reason
                .unwrap_or_else(|| format!("denied by hook '{hook_name}'")),
            hook_name: hook_name.to_string(),
        }),
        "allow" => Ok(HookDecision::Allow),
        other => Err(format!(
            "unknown decision value '{other}' from hook '{hook_name}'"
        )),
    }
}

/// JSON from `Stop`/`SubagentStop` gate hooks. All fields optional; one output
/// can combine several signals.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct StopHookJson {
    #[serde(default)]
    pub decision: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default, rename = "continue")]
    pub continue_: Option<bool>,
    #[serde(default, rename = "stopReason")]
    pub stop_reason: Option<String>,
    #[serde(default, rename = "hookSpecificOutput")]
    pub hook_specific_output: Option<StopHookSpecificOutputJson>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct StopHookSpecificOutputJson {
    #[serde(default, rename = "additionalContext")]
    pub additional_context: Option<String>,
}

/// Interpret a [`StopHookJson`] as a [`StopHookOutcome`].
///
/// `decision: "block"` requires a reason (a missing one falls back to a generic
/// message). `decision: "approve"` is a no-op; any other value is an error so
/// typos surface.
pub(crate) fn stop_json_to_outcome(
    json: StopHookJson,
    hook_name: &str,
) -> Result<StopHookOutcome, String> {
    let block_reason = match json.decision.as_deref() {
        Some("block") => Some(
            json.reason
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or_else(|| format!("Blocked by stop hook '{hook_name}'")),
        ),
        Some("approve") | None => None,
        Some(other) => {
            return Err(format!(
                "unknown decision value '{other}' from hook '{hook_name}'"
            ));
        }
    };
    Ok(StopHookOutcome {
        block_reason,
        additional_context: json
            .hook_specific_output
            .and_then(|output| output.additional_context)
            .filter(|context| !context.trim().is_empty()),
        force_stop: (json.continue_ == Some(false)).then_some(crate::result::StopOverride {
            reason: json.stop_reason,
        }),
    })
}

/// Each runner returns the result, wall-clock duration, and optional HTTP
/// metadata for enriched scrollback logging.
pub type HookRunOutput = (HookRunnerResult, Duration, Option<HttpInfo>);

pub async fn run_hook(
    spec: &HookSpec,
    envelope: &HookEventEnvelope,
    ctx: &RunContext<'_>,
    mode: GateKind,
) -> HookRunOutput {
    match spec.handler_type {
        crate::config::HandlerType::Command => {
            let (result, elapsed) = command::run_command_hook(spec, envelope, ctx, mode).await;
            (result, elapsed, None)
        }
        crate::config::HandlerType::Http => http::run_http_hook(spec, envelope, ctx, mode).await,
    }
}
