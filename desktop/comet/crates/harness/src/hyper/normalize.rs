//! ACP `SessionUpdate` → comet `AgentEvent` mapping.
//!
//! Reference: `xai-hyper-desktop/src/services/acp_backend.rs::session_notification`
//! already maps every ACP variant to a UI event; this is the same mapping
//! retargeted at comet's `AgentEvent`. `Done`/`AssistantMessageCompleted` are
//! NOT produced here — they come from the `session/prompt` await's `stop_reason`
//! (a separate path in `mod.rs`).

use agent_client_protocol as acp;
use comet_proto::agent::{AgentEvent, DoneStatus, HarnessId, TodoItem, ToolCall as CometToolCall};

/// Map one ACP `SessionUpdate` into zero or more `AgentEvent`s.
///
/// `assistant_message_id` is the id minted at `session/new`; `AssistantMessageCompleted`
/// is emitted from the prompt-resolution path, not here.
pub fn map_update(update: acp::SessionUpdate, _session_id: &acp::SessionId) -> Vec<AgentEvent> {
    match update {
        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk { content, .. }) => {
            if let acp::ContentBlock::Text(t) = content {
                if !t.text.is_empty() {
                    return vec![AgentEvent::TextDelta { text: t.text }];
                }
            }
            Vec::new()
        }
        acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk { content, .. }) => {
            if let acp::ContentBlock::Text(t) = content {
                if !t.text.is_empty() {
                    return vec![AgentEvent::ReasoningDelta { text: t.text }];
                }
            }
            Vec::new()
        }
        // User echo is written by the engine; ignore the live echo.
        acp::SessionUpdate::UserMessageChunk(_) => Vec::new(),

        acp::SessionUpdate::ToolCall(tc) => tool_call_events(
            &tc.tool_call_id.0,
            tc.status,
            tc.title.as_str(),
            tc.kind,
            tc.raw_input.as_ref(),
            &tc.locations,
        ),

        acp::SessionUpdate::ToolCallUpdate(upd) => tool_call_events(
            &upd.tool_call_id.0,
            upd.fields.status.unwrap_or(acp::ToolCallStatus::InProgress),
            // Updates often omit title — never replace a good name with "".
            upd.fields.title.as_deref().unwrap_or(""),
            upd.fields.kind.unwrap_or_default(),
            upd.fields.raw_input.as_ref(),
            upd.fields.locations.as_deref().unwrap_or(&[]),
        ),
        // Usage/other ACP updates have no direct comet equivalent here; usage is
        // not surfaced (comet parity exclusion: token-usage display).
        _ => Vec::new(),
    }
}

/// Emit a `ToolCall` (always) and, on a terminal status, a `ToolResult`.
fn tool_call_events(
    id: &str,
    status: acp::ToolCallStatus,
    title: &str,
    kind: acp::ToolKind,
    raw_input: Option<&serde_json::Value>,
    locations: &[acp::ToolCallLocation],
) -> Vec<AgentEvent> {
    let call = map_tool_call(title, kind, raw_input, locations);
    let mut out = vec![AgentEvent::ToolCall {
        id: id.to_string(),
        call,
    }];
    match status {
        acp::ToolCallStatus::Completed => out.push(AgentEvent::ToolResult {
            id: id.to_string(),
            is_error: false,
        }),
        acp::ToolCallStatus::Failed => out.push(AgentEvent::ToolResult {
            id: id.to_string(),
            is_error: true,
        }),
        _ => {}
    }
    out
}

/// Map an ACP tool call to a typed comet [`CometToolCall`] so the middle canvas
/// can show **name + detail** (Run/Read/Edit/…), not a blank "Tool".
fn map_tool_call(
    title: &str,
    kind: acp::ToolKind,
    raw_input: Option<&serde_json::Value>,
    locations: &[acp::ToolCallLocation],
) -> CometToolCall {
    let title = title.trim();
    let input = raw_input;
    let path_from_loc = locations
        .first()
        .map(|l| l.path.display().to_string())
        .filter(|s| !s.is_empty());

    // 1) Prefer decoding from raw JSON (stable field names from hyper tools).
    if let Some(v) = input {
        if let Some(call) = decode_from_input(title, v, path_from_loc.as_deref()) {
            return call;
        }
    }

    // 2) Title patterns hyper uses: `Execute \`cmd\``, tool function names, etc.
    if let Some(call) = decode_from_title(title, input, path_from_loc.as_deref()) {
        return call;
    }

    // 3) ACP ToolKind as a coarse type when we only have a path/location.
    if let Some(path) = path_from_loc.as_deref() {
        match kind {
            acp::ToolKind::Read => {
                return CometToolCall::ReadFile {
                    path: path.to_string(),
                };
            }
            acp::ToolKind::Edit | acp::ToolKind::Move => {
                return CometToolCall::EditFile {
                    path: path.to_string(),
                    old_string: None,
                    new_string: None,
                };
            }
            acp::ToolKind::Delete => {
                return CometToolCall::ApplyPatch {
                    path: Some(path.to_string()),
                };
            }
            _ => {}
        }
    }

    // 4) Fallback: keep a human-visible name (never empty "Tool").
    let name = if !title.is_empty() {
        title.to_string()
    } else {
        kind_label(kind).to_string()
    };
    CometToolCall::Unknown {
        name,
        input: input.cloned(),
    }
}

fn kind_label(kind: acp::ToolKind) -> &'static str {
    match kind {
        acp::ToolKind::Read => "read",
        acp::ToolKind::Edit => "edit",
        acp::ToolKind::Delete => "delete",
        acp::ToolKind::Move => "move",
        acp::ToolKind::Search => "search",
        acp::ToolKind::Execute => "run",
        acp::ToolKind::Think => "think",
        acp::ToolKind::Fetch => "fetch",
        acp::ToolKind::SwitchMode => "mode",
        acp::ToolKind::Other | _ => "tool",
    }
}

fn str_field(v: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(s) = v.get(*k).and_then(|x| x.as_str()).filter(|s| !s.is_empty()) {
            return Some(s.to_string());
        }
    }
    None
}

fn decode_from_input(
    title: &str,
    v: &serde_json::Value,
    loc_path: Option<&str>,
) -> Option<CometToolCall> {
    // Explicit tool name in payload / meta (when agents include it).
    let tool_name = str_field(v, &["name", "tool", "toolName", "tool_name", "function"])
        .or_else(|| {
            v.get("function")
                .and_then(|f| str_field(f, &["name"]))
        })
        .unwrap_or_default();
    let name_hint = if !tool_name.is_empty() {
        tool_name.as_str()
    } else {
        title
    };
    let name_l = name_hint.to_ascii_lowercase();

    // --- shell / exec ---
    if matches!(
        name_l.as_str(),
        "bash"
            | "shell"
            | "run_terminal_command"
            | "run_command"
            | "execute"
            | "exec"
            | "terminal"
            | "run"
    ) || str_field(v, &["command", "cmd", "shell_command"]).is_some()
    {
        if let Some(command) = str_field(v, &["command", "cmd", "shell_command", "script"]) {
            return Some(CometToolCall::Exec { command });
        }
    }

    // --- read ---
    if matches!(
        name_l.as_str(),
        "read" | "read_file" | "readfile" | "cat" | "open_file" | "view"
    ) {
        let path = str_field(v, &["path", "file", "file_path", "filename", "target_file"])
            .or_else(|| loc_path.map(str::to_string))?;
        return Some(CometToolCall::ReadFile { path });
    }

    // --- write ---
    if matches!(
        name_l.as_str(),
        "write" | "write_file" | "writefile" | "create_file" | "create"
    ) {
        let path = str_field(v, &["path", "file", "file_path", "filename", "target_file"])
            .or_else(|| loc_path.map(str::to_string))?;
        let content = str_field(v, &["content", "text", "new_string", "contents"]);
        return Some(CometToolCall::WriteFile { path, content });
    }

    // --- edit / str replace ---
    if matches!(
        name_l.as_str(),
        "edit"
            | "edit_file"
            | "str_replace"
            | "search_replace"
            | "apply_patch"
            | "applypatch"
            | "multiedit"
    ) {
        let path = str_field(v, &["path", "file", "file_path", "filename", "target_file"])
            .or_else(|| loc_path.map(str::to_string));
        if name_l.contains("patch") {
            return Some(CometToolCall::ApplyPatch { path });
        }
        let path = path?;
        return Some(CometToolCall::EditFile {
            path,
            old_string: str_field(v, &["old_string", "oldString", "old_str", "old"]),
            new_string: str_field(v, &["new_string", "newString", "new_str", "new"]),
        });
    }

    // --- grep / search ---
    if matches!(
        name_l.as_str(),
        "grep" | "search" | "rg" | "codebase_search" | "semantic_search"
    ) {
        let pattern =
            str_field(v, &["pattern", "query", "regex", "search", "needle"]).unwrap_or_default();
        if pattern.is_empty() && !name_l.is_empty() {
            // still ok — show tool name via Unknown path
        } else if !pattern.is_empty() {
            return Some(CometToolCall::Search {
                pattern,
                path: str_field(v, &["path", "glob", "directory", "cwd", "root"]),
            });
        }
    }

    // --- glob ---
    if matches!(name_l.as_str(), "glob" | "find_files" | "list_files") {
        if let Some(pattern) = str_field(v, &["pattern", "glob", "query"]) {
            return Some(CometToolCall::Glob { pattern });
        }
    }

    // --- web ---
    if matches!(name_l.as_str(), "web_fetch" | "webfetch" | "fetch" | "http_get") {
        if let Some(url) = str_field(v, &["url", "uri", "href"]) {
            return Some(CometToolCall::WebFetch {
                url,
                prompt: str_field(v, &["prompt", "query", "instruction"]),
            });
        }
    }
    if matches!(name_l.as_str(), "web_search" | "websearch" | "search_web") {
        if let Some(query) = str_field(v, &["query", "q", "search"]) {
            return Some(CometToolCall::WebSearch { query });
        }
    }

    // --- todos ---
    if matches!(name_l.as_str(), "todo" | "todo_write" | "update_todos" | "todowrite") {
        let items = parse_todo_items(v);
        return Some(CometToolCall::Todo { items });
    }

    // --- MCP style server__tool ---
    if let Some((server, tool)) = name_hint.split_once("__") {
        return Some(CometToolCall::Mcp {
            server: server.to_string(),
            tool: tool.to_string(),
            input: Some(v.clone()),
        });
    }
    if let (Some(server), Some(tool)) = (
        str_field(v, &["server", "mcp_server", "serverName"]),
        str_field(v, &["tool", "toolName", "tool_name"]),
    ) {
        return Some(CometToolCall::Mcp {
            server,
            tool,
            input: Some(v.clone()),
        });
    }

    None
}

fn decode_from_title(
    title: &str,
    input: Option<&serde_json::Value>,
    loc_path: Option<&str>,
) -> Option<CometToolCall> {
    if title.is_empty() {
        return None;
    }
    // `Execute \`ls -la\`` / `Execute 'ls'`
    if let Some(rest) = title
        .strip_prefix("Execute `")
        .or_else(|| title.strip_prefix("Execute '"))
        .or_else(|| title.strip_prefix("Run `"))
        .or_else(|| title.strip_prefix("Running `"))
    {
        let command = rest
            .trim_end_matches('`')
            .trim_end_matches('\'')
            .to_string();
        if !command.is_empty() {
            return Some(CometToolCall::Exec { command });
        }
    }
    // Bare tool id as title (hyper often does this): "grep", "read_file", …
    if let Some(v) = input {
        return decode_from_input(title, v, loc_path);
    }
    // Title-only with a path location.
    let name_l = title.to_ascii_lowercase();
    if let Some(path) = loc_path {
        if name_l.contains("read") {
            return Some(CometToolCall::ReadFile {
                path: path.to_string(),
            });
        }
        if name_l.contains("edit") || name_l.contains("write") {
            return Some(CometToolCall::EditFile {
                path: path.to_string(),
                old_string: None,
                new_string: None,
            });
        }
    }
    None
}

fn parse_todo_items(v: &serde_json::Value) -> Vec<TodoItem> {
    let arr = v
        .get("todos")
        .or_else(|| v.get("items"))
        .or_else(|| v.get("tasks"))
        .and_then(|x| x.as_array());
    let Some(arr) = arr else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            let text = str_field(item, &["text", "content", "title", "subject"])?;
            let done = item
                .get("done")
                .or_else(|| item.get("completed"))
                .and_then(|x| x.as_bool())
                .unwrap_or_else(|| {
                    item.get("status")
                        .and_then(|x| x.as_str())
                        .is_some_and(|s| {
                            matches!(s.to_ascii_lowercase().as_str(), "done" | "completed" | "complete")
                        })
                });
            Some(TodoItem { text, done })
        })
        .collect()
}

/// Translate the ACP `session/prompt` `stop_reason` + the harness `assistant_message_id`
/// into the terminal `AgentEvent`s: `AssistantMessageCompleted` then `Done`.
pub fn done_from_stop(
    stop_reason: &acp::PromptResponse,
    assistant_message_id: &str,
    session_id: &str,
) -> Vec<AgentEvent> {
    let mut out = vec![AgentEvent::AssistantMessageCompleted {
        assistant_message_id: assistant_message_id.to_string(),
    }];
    let status = match stop_reason.stop_reason {
        acp::StopReason::EndTurn => DoneStatus::Completed,
        acp::StopReason::Cancelled => DoneStatus::Interrupted,
        _ => DoneStatus::Errored,
    };
    out.push(AgentEvent::Done {
        status,
        result: None,
        error: None,
        session_id: Some(session_id.to_string()),
    });
    let _ = HarnessId::Hyper;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_bash_command() {
        let call = map_tool_call(
            "bash",
            acp::ToolKind::Execute,
            Some(&json!({"command": "ls -la"})),
            &[],
        );
        assert_eq!(
            call,
            CometToolCall::Exec {
                command: "ls -la".into()
            }
        );
    }

    #[test]
    fn maps_execute_title_pattern() {
        let call = map_tool_call(
            "Execute `cargo test`",
            acp::ToolKind::Execute,
            None,
            &[],
        );
        assert_eq!(
            call,
            CometToolCall::Exec {
                command: "cargo test".into()
            }
        );
    }

    #[test]
    fn maps_read_file() {
        let call = map_tool_call(
            "read_file",
            acp::ToolKind::Read,
            Some(&json!({"path": "src/main.rs"})),
            &[],
        );
        assert_eq!(
            call,
            CometToolCall::ReadFile {
                path: "src/main.rs".into()
            }
        );
    }

    #[test]
    fn maps_grep() {
        let call = map_tool_call(
            "grep",
            acp::ToolKind::Search,
            Some(&json!({"pattern": "foo", "path": "src"})),
            &[],
        );
        assert_eq!(
            call,
            CometToolCall::Search {
                pattern: "foo".into(),
                path: Some("src".into()),
            }
        );
    }

    #[test]
    fn empty_title_still_gets_kind_name() {
        let call = map_tool_call("", acp::ToolKind::Search, None, &[]);
        match call {
            CometToolCall::Unknown { name, .. } => assert_eq!(name, "search"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn maps_mcp_double_underscore() {
        let call = map_tool_call(
            "linear__save_issue",
            acp::ToolKind::Other,
            Some(&json!({"title": "x"})),
            &[],
        );
        assert_eq!(
            call,
            CometToolCall::Mcp {
                server: "linear".into(),
                tool: "save_issue".into(),
                input: Some(json!({"title": "x"})),
            }
        );
    }
}
