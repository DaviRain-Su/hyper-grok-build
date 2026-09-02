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
    // Rebuilt and VerbatimMirror children both drop the active-message tools, which only the root session may use
    // The filter matches by kind so a renamed tool is still caught, and by canonical name when the child bridge no longer registers the tool
    // VerbatimMirror leaves every other field of the parent's ToolSpecs unchanged so the child's request still hits the parent's radix cache
    // ask_user_question is stripped at the subagent mirror call sites, so forks that are not subagents keep it
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
