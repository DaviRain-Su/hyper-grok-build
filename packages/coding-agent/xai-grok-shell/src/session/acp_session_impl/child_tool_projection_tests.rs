use super::*;

fn tool(name: &str, description: Option<&str>, parameters: serde_json::Value) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.map(str::to_owned),
        parameters,
    }
}

fn specs() -> Vec<ToolSpec> {
    vec![
        tool(
            "read_file",
            Some("read"),
            serde_json::json!({"type": "object"}),
        ),
        tool(
            "relay_to_subagent",
            Some("renamed message"),
            serde_json::json!({"required": ["text"]}),
        ),
        tool(
            "grep",
            None,
            serde_json::json!({"type": "object", "required": ["pattern"]}),
        ),
    ]
}

fn kind_for_name(name: &str) -> Option<ToolKind> {
    (name == "relay_to_subagent").then_some(ToolKind::ActiveAgentMessage)
}

#[test]
fn rebuilt_projection_removes_renamed_active_message_tool() {
    let projected = child_safe_tool_specs(specs(), ChildToolProjection::Rebuilt, kind_for_name);

    assert_eq!(projected.len(), 2);
    assert_eq!(projected[0].name, "read_file");
    assert_eq!(projected[0].description.as_deref(), Some("read"));
    assert_eq!(projected[1].name, "grep");
    assert_eq!(
        projected[1].parameters,
        serde_json::json!({"type": "object", "required": ["pattern"]})
    );
}

#[test]
fn verbatim_mirror_projection_strips_root_only_keeps_ordinary_byte_identical() {
    // Ordinary tools pass through unchanged, so the child's specs serialize to the parent's exact bytes and the radix cache stays aligned
    // The ActiveAgentMessage tool exists only at the root, so even the mirror drops it: by kind when renamed, by canonical name with no kind known
    let parent = vec![
        tool(
            "read_file",
            Some("read"),
            serde_json::json!({"type": "object"}),
        ),
        tool(
            "relay_to_subagent",
            Some("renamed message"),
            serde_json::json!({"required": ["text"]}),
        ),
        tool(
            SEND_SUBAGENT_MESSAGE_TOOL_NAME,
            Some("canonical message"),
            serde_json::json!({"required": ["subagent_id", "message"]}),
        ),
        tool(
            "grep",
            None,
            serde_json::json!({"type": "object", "required": ["pattern"]}),
        ),
    ];

    let projected = child_safe_tool_specs(
        parent.clone(),
        ChildToolProjection::VerbatimMirror,
        kind_for_name,
    );
    let expected = vec![parent[0].clone(), parent[3].clone()];

    assert_eq!(
        projected
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        vec!["read_file", "grep"]
    );
    assert_eq!(
        serde_json::to_vec(&projected).unwrap(),
        serde_json::to_vec(&expected).unwrap()
    );

    // With the kind lookup returning None, only the canonical-name check is left to drop the tool
    let name_only = child_safe_tool_specs(
        vec![
            tool("read_file", Some("read"), serde_json::json!({})),
            tool(
                SEND_SUBAGENT_MESSAGE_TOOL_NAME,
                Some("canonical"),
                serde_json::json!({}),
            ),
        ],
        ChildToolProjection::VerbatimMirror,
        |_| None,
    );
    assert_eq!(
        name_only
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        vec!["read_file"]
    );
}

#[test]
fn verbatim_mirror_path_strips_ask_user_and_active_message() {
    // Runs the same two steps as production spawn: child_safe_tool_specs, then strip_ask_user_question_tool
    let parent = vec![
        tool(
            "read_file",
            Some("read"),
            serde_json::json!({"type": "object"}),
        ),
        tool(
            "ask_user_question",
            Some("ask"),
            serde_json::json!({"type": "object"}),
        ),
        tool(
            SEND_SUBAGENT_MESSAGE_TOOL_NAME,
            Some("message"),
            serde_json::json!({"type": "object"}),
        ),
        tool(
            "relay_to_subagent",
            Some("renamed"),
            serde_json::json!({"type": "object"}),
        ),
        tool(
            "grep",
            None,
            serde_json::json!({"type": "object", "required": ["pattern"]}),
        ),
    ];

    let mut tools =
        child_safe_tool_specs(parent, ChildToolProjection::VerbatimMirror, kind_for_name);
    crate::agent::subagent::strip_ask_user_question_tool(&mut tools);

    assert_eq!(
        tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        vec!["read_file", "grep"]
    );
    assert!(tools.iter().all(|t| {
        t.name != "ask_user_question"
            && t.name != SEND_SUBAGENT_MESSAGE_TOOL_NAME
            && t.name != "relay_to_subagent"
    }));
}
