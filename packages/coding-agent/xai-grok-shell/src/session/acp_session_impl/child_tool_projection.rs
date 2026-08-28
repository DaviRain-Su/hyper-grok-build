//! Model-facing child tool projection with verbatim-fork cache preservation.

use xai_grok_sampling_types::ToolSpec;
use xai_grok_tools::implementations::grok_build::SEND_SUBAGENT_MESSAGE_TOOL_NAME;
use xai_grok_tools::types::tool::ToolKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChildToolProjection {
    Rebuilt,
    VerbatimMirror,
}

pub(super) fn child_safe_tool_specs(
    specs: Vec<ToolSpec>,
    projection: ChildToolProjection,
    kind_for_name: impl Fn(&str) -> Option<ToolKind>,
) -> Vec<ToolSpec> {
    // Both rebuilt and verbatim-mirror children drop root-only active-message
    // tools (by kind for renames, and by canonical name when the child bridge
    // no longer registers the tool). VerbatimMirror still preserves every
    // other parent ToolSpec field for radix-cache alignment; ask_user_question
    // is stripped at the subagent mirror call sites so non-subagent forks keep it.
    match projection {
        ChildToolProjection::Rebuilt | ChildToolProjection::VerbatimMirror => specs
            .into_iter()
            .filter(|spec| {
                kind_for_name(&spec.name) != Some(ToolKind::ActiveAgentMessage)
                    && spec.name != SEND_SUBAGENT_MESSAGE_TOOL_NAME
            })
            .collect(),
    }
}

#[cfg(test)]
#[path = "child_tool_projection_tests.rs"]
mod tests;
