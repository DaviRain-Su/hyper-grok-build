//! Resolve internal URLs to virtual file content.

use std::path::{Path, PathBuf};

use super::conflict::{ConflictRegistryResource, ConflictSide};
use super::schemes::{InternalScheme, InternalUrl, parse_internal_url};

/// Context needed to resolve session-scoped schemes.
#[derive(Debug, Clone)]
pub struct ResolveContext {
    /// Parent (or current root) session id.
    pub session_id: String,
    /// Session working directory (for sessions path encoding).
    pub cwd: PathBuf,
    /// Optional conflict registry for `conflict://`.
    pub conflicts: Option<ConflictRegistryResource>,
}

/// Successful virtual read.
#[derive(Debug, Clone)]
pub struct VirtualRead {
    pub display_path: String,
    pub text: String,
}

/// Apply 1-based offset/limit to raw text (same semantics as read_file lines).
pub fn apply_line_window(text: &str, offset: Option<i64>, limit: Option<usize>) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let start = offset
        .map(|o| if o <= 0 { 0 } else { (o as usize).saturating_sub(1) })
        .unwrap_or(0)
        .min(lines.len());
    let end = match limit {
        Some(n) => (start + n).min(lines.len()),
        None => lines.len(),
    };
    lines[start..end].join("\n")
}

/// If `path` is an internal URL, resolve it. Returns `None` for normal paths.
pub fn resolve_virtual_path(
    path: &str,
    ctx: &ResolveContext,
) -> Option<Result<VirtualRead, String>> {
    let url = parse_internal_url(path)?;
    Some(resolve_url(&url, ctx))
}

fn resolve_url(url: &InternalUrl, ctx: &ResolveContext) -> Result<VirtualRead, String> {
    match url.scheme {
        InternalScheme::Agent => resolve_agent(&url.rest, ctx),
        InternalScheme::History => resolve_history(&url.rest, ctx),
        InternalScheme::Conflict => resolve_conflict_read(&url.rest, ctx),
    }
}

fn parent_session_dir(ctx: &ResolveContext) -> PathBuf {
    let cwd = ctx.cwd.to_string_lossy();
    crate::util::grok_home::sessions_cwd_dir(cwd.as_ref()).join(&ctx.session_id)
}

fn subagent_dir(ctx: &ResolveContext, id: &str) -> PathBuf {
    parent_session_dir(ctx).join("subagents").join(id)
}

fn resolve_agent(id: &str, ctx: &ResolveContext) -> Result<VirtualRead, String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("agent:// requires a subagent id (use history:// to list)".into());
    }
    if !is_safe_id(id) {
        return Err(format!("invalid agent id {id:?}"));
    }
    let dir = subagent_dir(ctx, id);
    let text = read_subagent_output_text(&dir)
        .ok_or_else(|| format!("Not found: agent://{id} (no output.json under {})", dir.display()))?;
    Ok(VirtualRead {
        display_path: format!("agent://{id}"),
        text,
    })
}

fn read_subagent_output_text(dir: &Path) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct OutputFile {
        #[serde(default)]
        schema_version: u32,
        output: String,
    }
    let data = std::fs::read_to_string(dir.join("output.json")).ok()?;
    let file: OutputFile = serde_json::from_str(&data).ok()?;
    // Accept v1 (current shell) and bare future versions that still have `output`.
    if file.schema_version == 0 || file.schema_version == 1 {
        Some(file.output)
    } else {
        Some(file.output)
    }
}

fn resolve_history(rest: &str, ctx: &ResolveContext) -> Result<VirtualRead, String> {
    let rest = rest.trim();
    if rest.is_empty() {
        return Ok(VirtualRead {
            display_path: "history://".into(),
            text: list_subagents_roster(ctx),
        });
    }
    if !is_safe_id(rest) {
        return Err(format!("invalid history id {rest:?}"));
    }
    let dir = subagent_dir(ctx, rest);
    let meta = read_meta_json(&dir);
    let status = meta
        .as_ref()
        .and_then(|m| m.get("status"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let subagent_type = meta
        .as_ref()
        .and_then(|m| m.get("subagent_type"))
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let child_session_id = meta
        .as_ref()
        .and_then(|m| m.get("child_session_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let mut out = String::new();
    out.push_str(&format!(
        "# history://{rest}\n\n- type: {subagent_type}\n- status: {status}\n"
    ));
    if !child_session_id.is_empty() {
        out.push_str(&format!("- child_session_id: {child_session_id}\n"));
    }
    out.push('\n');

    // Prefer child session chat history when we can locate it.
    if !child_session_id.is_empty() {
        if let Some(transcript) = find_chat_history_for_session(child_session_id) {
            out.push_str("## Transcript (concise)\n\n");
            out.push_str(&render_concise_transcript(&transcript));
            return Ok(VirtualRead {
                display_path: format!("history://{rest}"),
                text: out,
            });
        }
    }

    // Fallback: final output blob if any.
    if let Some(output) = read_subagent_output_text(&dir) {
        out.push_str("## Final output\n\n");
        out.push_str(&output);
        out.push('\n');
        return Ok(VirtualRead {
            display_path: format!("history://{rest}"),
            text: out,
        });
    }

    if meta.is_none() {
        return Err(format!(
            "Not found: history://{rest} (no subagent meta under {})",
            dir.display()
        ));
    }
    out.push_str("_No transcript or output available yet._\n");
    Ok(VirtualRead {
        display_path: format!("history://{rest}"),
        text: out,
    })
}

fn list_subagents_roster(ctx: &ResolveContext) -> String {
    let root = parent_session_dir(ctx).join("subagents");
    let mut lines = vec![
        "# history:// roster".to_string(),
        String::new(),
        format!("Parent session: {}", ctx.session_id),
        String::new(),
    ];
    let Ok(entries) = std::fs::read_dir(&root) else {
        lines.push("_No subagents directory (none spawned yet)._".into());
        return lines.join("\n");
    };
    let mut rows = Vec::new();
    for ent in entries.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if !ent.path().is_dir() || !is_safe_id(&name) {
            continue;
        }
        let meta = read_meta_json(&ent.path());
        let status = meta
            .as_ref()
            .and_then(|m| m.get("status"))
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let ty = meta
            .as_ref()
            .and_then(|m| m.get("subagent_type"))
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let has_out = ent.path().join("output.json").is_file();
        rows.push(format!(
            "- `{name}` [{ty}] status={status}{} → history://{name}{}",
            if has_out { " has_output" } else { "" },
            if has_out {
                format!(" · agent://{name}")
            } else {
                String::new()
            }
        ));
    }
    rows.sort();
    if rows.is_empty() {
        lines.push("_No subagents registered._".into());
    } else {
        lines.extend(rows);
    }
    lines.join("\n")
}

fn read_meta_json(dir: &Path) -> Option<serde_json::Value> {
    let data = std::fs::read_to_string(dir.join("meta.json")).ok()?;
    serde_json::from_str(&data).ok()
}

/// Scan under `~/.grok/sessions` for a session dir whose name is `session_id`
/// and load `chat_history.jsonl` (first hit).
fn find_chat_history_for_session(session_id: &str) -> Option<String> {
    if !is_safe_id(session_id) {
        return None;
    }
    let sessions = crate::util::grok_home::grok_home().join("sessions");
    let max_depth = sessions.components().count() + 4;
    let mut stack = vec![sessions];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for ent in rd.flatten() {
            let p = ent.path();
            if p.is_dir() {
                if p.file_name().and_then(|n| n.to_str()) == Some(session_id) {
                    let chat = p.join("chat_history.jsonl");
                    if chat.is_file() {
                        return std::fs::read_to_string(chat).ok();
                    }
                }
                // Bound depth: sessions/<cwd-enc>/<id>
                if p.components().count() < max_depth {
                    stack.push(p);
                }
            }
        }
    }
    None
}

/// Collapse JSONL conversation into short markdown for model consumption.
fn render_concise_transcript(jsonl: &str) -> String {
    let mut out = String::new();
    let mut n = 0usize;
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        n += 1;
        if n > 200 {
            out.push_str("\n_…truncated_\n");
            break;
        }
        // Support both ConversationItem-shaped and simple role/content rows.
        if let Some(role) = v.get("role").and_then(|r| r.as_str()) {
            let content = extract_text_content(&v);
            match role {
                "user" | "User" => {
                    out.push_str(&format!("**user:** {}\n\n", truncate_one_line(&content, 400)));
                }
                "assistant" | "Assistant" => {
                    out.push_str(&format!(
                        "**assistant:** {}\n\n",
                        truncate_one_line(&content, 600)
                    ));
                    if let Some(tools) = v.get("tool_calls").and_then(|t| t.as_array()) {
                        for tc in tools {
                            let name = tc
                                .get("function")
                                .and_then(|f| f.get("name"))
                                .or_else(|| tc.get("name"))
                                .and_then(|n| n.as_str())
                                .unwrap_or("tool");
                            out.push_str(&format!("// tool: {name}\n"));
                        }
                        out.push('\n');
                    }
                }
                "tool" | "ToolResult" => {
                    let name = v
                        .get("name")
                        .or_else(|| v.get("tool_name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("tool");
                    out.push_str(&format!(
                        "// result: {name} — {}\n\n",
                        truncate_one_line(&content, 200)
                    ));
                }
                "system" | "System" => {
                    // skip bulk system prompts
                }
                _ => {}
            }
            continue;
        }
        // Tagged ConversationItem style: {"type":"user", ...}
        if let Some(ty) = v.get("type").and_then(|t| t.as_str()) {
            match ty {
                "user" | "User" => {
                    let content = extract_text_content(&v);
                    out.push_str(&format!("**user:** {}\n\n", truncate_one_line(&content, 400)));
                }
                "assistant" | "Assistant" => {
                    let content = extract_text_content(&v);
                    out.push_str(&format!(
                        "**assistant:** {}\n\n",
                        truncate_one_line(&content, 600)
                    ));
                }
                "tool_result" | "ToolResult" => {
                    let name = v
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("tool");
                    let content = extract_text_content(&v);
                    out.push_str(&format!(
                        "// result: {name} — {}\n\n",
                        truncate_one_line(&content, 200)
                    ));
                }
                _ => {}
            }
        }
    }
    if out.is_empty() {
        out.push_str("_Empty transcript._\n");
    }
    out
}

fn extract_text_content(v: &serde_json::Value) -> String {
    if let Some(s) = v.get("content").and_then(|c| c.as_str()) {
        return s.to_string();
    }
    if let Some(s) = v.get("text").and_then(|c| c.as_str()) {
        return s.to_string();
    }
    if let Some(arr) = v.get("content").and_then(|c| c.as_array()) {
        let mut parts = Vec::new();
        for item in arr {
            if let Some(s) = item.as_str() {
                parts.push(s.to_string());
            } else if let Some(s) = item.get("text").and_then(|t| t.as_str()) {
                parts.push(s.to_string());
            }
        }
        return parts.join("\n");
    }
    String::new()
}

fn truncate_one_line(s: &str, max: usize) -> String {
    let flat: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if flat.chars().count() <= max {
        flat
    } else {
        let t: String = flat.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

fn resolve_conflict_read(rest: &str, ctx: &ResolveContext) -> Result<VirtualRead, String> {
    let some_reg = ctx
        .conflicts
        .as_ref()
        .ok_or_else(|| "conflict:// unavailable (ConflictRegistryResource not installed)".to_string())?;
    let reg = some_reg.lock();

    let rest = rest.trim();
    if rest.is_empty() || rest == "*" {
        let list = reg.list();
        if list.is_empty() {
            return Ok(VirtualRead {
                display_path: "conflict://".into(),
                text: "No registered conflicts. Read a file with `:conflicts` suffix or re-scan markers.".into(),
            });
        }
        let mut text = String::from("# Registered conflicts\n\n");
        for c in list {
            text.push_str(&format!(
                "- conflict://{} → {} (bytes {}..{})\n",
                c.id,
                c.file_path.display(),
                c.start_byte,
                c.end_byte
            ));
        }
        return Ok(VirtualRead {
            display_path: "conflict://".into(),
            text,
        });
    }

    // conflict://N or conflict://N/ours
    let mut parts = rest.splitn(2, '/');
    let id_str = parts.next().unwrap_or("");
    let side = parts.next();
    let id: u32 = id_str
        .parse()
        .map_err(|_| format!("invalid conflict id {id_str:?}"))?;
    let c = reg
        .get(id)
        .ok_or_else(|| format!("unknown conflict id {id}; list via conflict://"))?;

    let text = match side {
        None => c.marker_block.clone(),
        Some(s) => {
            let side = ConflictSide::parse(s)
                .ok_or_else(|| format!("unknown conflict side {s:?}; use ours|theirs|base|both"))?;
            match side {
                ConflictSide::Ours => c.ours.clone(),
                ConflictSide::Theirs => c.theirs.clone(),
                ConflictSide::Base => c
                    .base
                    .clone()
                    .unwrap_or_else(|| "(no base section)".into()),
                ConflictSide::Both => format!("{}{}", c.ours, c.theirs),
            }
        }
    };
    Ok(VirtualRead {
        display_path: format!("conflict://{rest}"),
        text,
    })
}

fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() < 200
        && !id.contains("..")
        && !id.contains('/')
        && !id.contains('\\')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::internal_urls::conflict::ConflictRegistryResource;

    #[test]
    fn agent_and_history_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let _cwd = tmp.path().to_path_buf();
        // Exercise apply_line_window + parse path (disk session layout is
        // covered by integration tests against real session dirs).
        let text = "a\nb\nc\nd\n";
        assert_eq!(apply_line_window(text, Some(2), Some(2)), "b\nc");

        let url = parse_internal_url("agent://x").unwrap();
        assert_eq!(url.rest, "x");

        let _ = ConflictRegistryResource::new();
    }

    #[test]
    fn safe_id() {
        assert!(is_safe_id("sa-1"));
        assert!(!is_safe_id("../x"));
        assert!(!is_safe_id("a/b"));
    }
}
