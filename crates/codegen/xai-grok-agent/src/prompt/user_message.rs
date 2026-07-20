//! Per-agent first-user-message rendering.
//!
//! Mirrors `prompt::context::PromptContext` but for the first user message
//! (the prefix that contains `<user_info>`, `<git_status>`, optional
//! workspace overview, optional rules / skills / MCP listings).
//!
//! `UserMessageTemplate` selects the rendering strategy:
//! - `Default` -- the legacy Grok Build prefix (built by the shell layer).
//! - `Custom`  -- caller-supplied template string (MiniJinja, same delimiters
//!   as the system prompt templates).
//!
//! The shell layer gathers session-scoped inputs (cwd, vcs status, rule
//! files, skill registry, MCP servers) and hands them to
//! `UserMessageContext::render`, which dispatches on `template`.
use crate::prompt::agents_md::AgentConfigFile;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use xai_grok_tools::bridge::ToolBridge;
use xai_grok_tools::implementations::skills::types::SkillInfo;
use xai_grok_tools::types::skill_discovery_tracker::{XmlRenderMode, format_announcement_xml};
/// Date format for the `Today's date` field of the user-message preamble
/// (e.g. "Friday Apr 24, 2026"). Any format change is observable to the model.
pub const USER_MESSAGE_DATE_FORMAT: &str = "%A %b %-d, %Y";
/// Per-repo character cap applied to `vcs_status` at render time. The
/// `<git_status>` block has no token budget -- this character cap is the only
/// size control, and it is applied per repo at render, never at gather, so
/// other consumers of the raw status are unaffected.
pub const GIT_STATUS_CHARACTER_LIMIT: usize = 10_000;
/// Trim, drop-if-empty, and cap a VCS status string for the
/// `<git_status>` block.
///
/// Returns `None` when the trimmed status is empty (so the section is dropped
/// and no empty code fence is emitted), otherwise the status capped at
/// [`GIT_STATUS_CHARACTER_LIMIT`] -- snapped back to the last newline -- with
/// the `... (git status truncated)` marker appended.
pub fn normalize_git_status(status: &str) -> Option<String> {
    let status = status.trim();
    if status.is_empty() {
        return None;
    }
    if status.len() <= GIT_STATUS_CHARACTER_LIMIT {
        return Some(status.to_string());
    }
    let mut end = GIT_STATUS_CHARACTER_LIMIT;
    while !status.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = &status[..end];
    if let Some(nl) = truncated.rfind('\n')
        && nl > 0
    {
        truncated = &truncated[..nl];
    }
    Some(format!("{truncated}\n\n... (git status truncated)"))
}
/// Selects the first-user-message rendering strategy for an agent.
///
/// Built-in variants decrypt the underlying XOR-obfuscated template on demand
/// (obfuscation, not security). Decrypted bytes are zeroed on drop via
/// `Zeroizing`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UserMessageTemplate {
    /// Legacy Grok Build prefix: `<user_info>` + optional `<git_status>`.
    /// Built directly by the shell layer; this
    ///   renderer returns `None` for `Default` and the caller falls back to
    ///   its own legacy path.
    #[default]
    Default,
    /// Caller-supplied MiniJinja template string.
    Custom(String),
}
impl UserMessageTemplate {
    pub fn is_cursor(&self) -> bool {
        false
    }
}
/// Backward-compatible deserialization: accepts both the new tagged format
/// (`"default"`, `{"custom": "..."}`) and a bare string (treated
/// as `Custom`).
impl<'de> Deserialize<'de> for UserMessageTemplate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = UserMessageTemplate;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(r#""default", "cursor", {"custom": "..."}, or a template string"#)
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                match v {
                    "default" => Ok(UserMessageTemplate::Default),
                    other => Ok(UserMessageTemplate::Custom(other.to_owned())),
                }
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                match map.next_key::<String>()? {
                    Some(ref k) if k == "custom" => {
                        let val: String = map.next_value()?;
                        Ok(UserMessageTemplate::Custom(val))
                    }
                    Some(other) => Err(serde::de::Error::unknown_field(&other, &["custom"])),
                    None => Err(serde::de::Error::custom(r#"expected {"custom": "..."}"#)),
                }
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}
/// One discovered rule file (AGENTS.md / Claude.md / .grok/rules/*.md).
///
/// Wire-compatible with `AgentConfigFile` -- this type exists so the
/// `UserMessageContext` does not depend on the AGENTS-discovery internals
/// beyond the path/content pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleEntry {
    /// Absolute path of the file (used as the rule `name` attribute).
    pub path: String,
    /// Raw file body.
    pub content: String,
}
impl From<AgentConfigFile> for RuleEntry {
    fn from(f: AgentConfigFile) -> Self {
        Self {
            path: f.file_path,
            content: f.content,
        }
    }
}
/// Connected MCP server metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerEntry {
    pub name: String,
    /// Free-form usage instructions a user provided when configuring the
    /// server. Surfaced in the `serverUseInstructions` attribute.
    pub server_use_instructions: Option<String>,
    /// Absolute path to the per-server descriptor folder. Surfaced in
    /// the `folderPath` attribute. Compatible models read tool
    /// schemas from `<folder_path>/tools/<tool>.json` and resource
    /// descriptors from `<folder_path>/resources/<resource>.json` before
    /// calling `CallMcpTool`/`FetchMcpResource`. The session is
    /// responsible for materializing the descriptor files at this path.
    pub folder_path: Option<String>,
}
/// All inputs the templated first user message needs. The shell gathers
/// these once at session start (and again on compaction) and hands the
/// struct to `render`.
#[derive(Debug, Clone)]
pub struct UserMessageContext {
    pub template: UserMessageTemplate,
    /// Display path -- the path the model sees as the workspace.
    pub workspace_path: PathBuf,
    /// OS identifier surfaced as the `<user_info>` `OS Version:` value.
    ///
    /// This is `"<kernel> <release>"` (e.g. `"darwin 24.6.0"`,
    /// `"linux 6.5.0-..."`) -- not the OS family (`std::env::consts::OS`, e.g.
    /// `"macos"`). Producers that don't have a uname-style string available may
    /// pass `std::env::consts::OS` as a fallback; callers that need the full
    /// string should use `xai_grok_shell::util::uname::os_kernel_and_release`
    /// (or equivalent).
    pub os_family: String,
    /// `$SHELL` env, basename only -- e.g. "zsh", "bash".
    pub shell: String,
    /// Git/jj working-tree root, if any.
    pub vcs_root: Option<PathBuf>,
    /// Pre-fetched VCS status output (caller handles timeouts).
    pub vcs_status: Option<String>,
    /// Local date captured at session start (or compaction). Formatted
    /// inside the renderer using [`USER_MESSAGE_DATE_FORMAT`] so the producer
    /// cannot accidentally drift the model-facing date shape.
    pub today_local: Option<NaiveDate>,
    /// Per-workspace terminals folder, surfaced as
    /// `Terminals folder: <path>` in the `<user_info>` block. The
    /// shell tool persists each background command's output to a file
    /// here (`<terminals_folder>/<numeric-shell-id>.txt`); the model uses
    /// this path to read terminal state via the read tool. Optional --
    /// when `None`, the line is omitted from the rendered preamble.
    pub terminals_folder: Option<PathBuf>,
    /// Workspace-scoped rule files (cwd / repo root / optional workspace user dir).
    pub workspace_rules: Vec<RuleEntry>,
    /// User-scoped rule files (~/.grok/, ~/.claude/).
    pub user_rules: Vec<RuleEntry>,
    /// Skill registry snapshot (already deduped). Rendered through the
    /// shared budget-tier renderer.
    pub skills: Vec<SkillInfo>,
    /// Optional listing budget in characters; defaults to the standard
    /// 1%-of-context heuristic when None.
    pub skill_listing_budget_chars: Option<usize>,
    /// Connected MCP servers (alphabetical).
    pub mcp_servers: Vec<McpServerEntry>,
    /// Absolute path to the per-workspace MCP descriptor root
    /// (`~/.grok/projects/<encoded-cwd>/mcps`). Surfaced in
    /// the `<mcp_file_system>` instructions so the model knows where
    /// to discover tool/resource schemas. Required when `mcp_servers` is
    /// non-empty; ignored otherwise.
    pub mcps_root: Option<String>,
    /// Client-facing name of the read tool (resolved from `TemplateRenderer`).
    /// Used in the skill section's instructional text. Defaults to `"Read"`.
    pub read_tool_name: String,
}
/// Typed placeholder bag handed to MiniJinja.
///
/// Field names here must match `${{ … }}` references in any caller-supplied
/// `Custom` template. Keeping this as a typed
/// struct -- rather than a free-form `serde_json::Value` -- means the set
/// of supported placeholders is greppable from one place, every nested
/// shape is enforced by `Serialize`, and rename refactors flow through
/// the compiler instead of silently producing empty strings at render
/// time.
#[derive(Debug, Clone, Serialize)]
struct UserMessagePlaceholders<'a> {
    workspace_path: String,
    os_family: &'a str,
    shell: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    vcs_root: Option<String>,
    /// Owned because the renderer caps/normalizes the raw status via
    /// [`normalize_git_status`] before handing it to MiniJinja.
    #[serde(skip_serializing_if = "Option::is_none")]
    vcs_status: Option<String>,
    /// Pre-formatted using [`USER_MESSAGE_DATE_FORMAT`]; `None` is rendered as
    /// `null` so the `${% if today_local %}` guard in the template drops
    /// the line entirely.
    #[serde(skip_serializing_if = "Option::is_none")]
    today_local: Option<String>,
    /// Pre-rendered as a string so the template can `${% if terminals_folder %}`-guard.
    #[serde(skip_serializing_if = "Option::is_none")]
    terminals_folder: Option<String>,
    has_rules: bool,
    workspace_rules: &'a [RuleEntry],
    user_rules: &'a [RuleEntry],
    /// Pre-rendered budgeted `<agent_skill>` XML rows; the template
    /// just substitutes this verbatim. See `render_skill_listing_xml` for
    /// why the skill listing is special-cased.
    skill_listing: String,
    /// Client-facing name of the read tool, used in the skill section's
    /// instructional text. Defaults to `"Read"`.
    read_tool_name: String,
    mcp_servers: &'a [McpServerEntry],
    #[serde(skip_serializing_if = "Option::is_none")]
    mcps_root: Option<&'a str>,
}
impl UserMessageContext {
    /// Build placeholders for MiniJinja rendering.
    fn placeholders(&self) -> UserMessagePlaceholders<'_> {
        UserMessagePlaceholders {
            workspace_path: self.workspace_path.to_string_lossy().into_owned(),
            os_family: &self.os_family,
            shell: &self.shell,
            vcs_root: self
                .vcs_root
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            vcs_status: self.vcs_status.as_deref().and_then(normalize_git_status),
            today_local: self
                .today_local
                .map(|d| d.format(USER_MESSAGE_DATE_FORMAT).to_string()),
            terminals_folder: self
                .terminals_folder
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            has_rules: !self.workspace_rules.is_empty() || !self.user_rules.is_empty(),
            workspace_rules: &self.workspace_rules,
            user_rules: &self.user_rules,
            skill_listing: self.render_skill_listing_xml().unwrap_or_default(),
            read_tool_name: self.read_tool_name.clone(),
            mcp_servers: &self.mcp_servers,
            mcps_root: self.mcps_root.as_deref(),
        }
    }
    /// Render the skill list as `<agent_skill>` XML rows.
    pub fn render_skill_listing_xml(&self) -> Option<String> {
        if self.skills.is_empty() {
            return None;
        }
        let mode = if self.template.is_cursor() {
            XmlRenderMode::Verbatim
        } else {
            XmlRenderMode::Budgeted {
                budget_chars: self.skill_listing_budget_chars,
                overflow_indicator: true,
            }
        };
        let mut announced = HashSet::new();
        format_announcement_xml(&self.skills, &mut announced, None, None, mode)
    }
    /// Render the first user message.
    ///
    /// Returns `None` for `UserMessageTemplate::Default` -- the caller is
    /// responsible for the legacy prefix path. `Custom` dispatches through
    /// `ToolBridge::render_prompt` so MiniJinja
    /// `${{ tools.by_kind.* }}` references resolve correctly.
    pub async fn render(&self, bridge: &ToolBridge) -> Option<String> {
        let placeholders = serde_json::to_value(self.placeholders())
            .expect("UserMessagePlaceholders serializes infallibly");
        let rendered = match &self.template {
            UserMessageTemplate::Default => return None,
            UserMessageTemplate::Custom(s) => bridge.render_prompt(s, &placeholders).await,
        };
        rendered.map(|s| s.trim_end().to_string())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn template_override_deserialize_strings() {
        let v: UserMessageTemplate = serde_json::from_str(r#""default""#).unwrap();
        assert_eq!(v, UserMessageTemplate::Default);
        let v: UserMessageTemplate = serde_json::from_str(r#""my custom""#).unwrap();
        assert_eq!(v, UserMessageTemplate::Custom("my custom".into()));
    }
    #[test]
    fn template_override_deserialize_custom_map() {
        let v: UserMessageTemplate =
            serde_json::from_str(r#"{"custom": "my template body"}"#).unwrap();
        assert_eq!(v, UserMessageTemplate::Custom("my template body".into()));
    }
    #[test]
    fn template_override_round_trip() {
        for original in [
            UserMessageTemplate::Default,
            UserMessageTemplate::Custom("body".into()),
        ] {
            let json = serde_json::to_string(&original).unwrap();
            let loaded: UserMessageTemplate = serde_json::from_str(&json).unwrap();
            assert_eq!(original, loaded);
        }
    }
    /// A status under the cap passes through unchanged (trim is a no-op for
    /// real `git status --short --branch` output, which starts with `##`).
    #[test]
    fn normalize_git_status_passthrough_under_limit() {
        let status = "## main...origin/main\n M src/app.rs";
        assert_eq!(normalize_git_status(status).as_deref(), Some(status));
    }
    /// Empty / whitespace-only status -> `None` so the section is dropped and
    /// no empty fence is emitted.
    #[test]
    fn normalize_git_status_drops_whitespace_only() {
        assert_eq!(normalize_git_status(""), None);
        assert_eq!(normalize_git_status("   \n\t  "), None);
    }
    /// A status over the cap is truncated at the last newline before the limit
    /// and carries the spec's truncation marker.
    #[test]
    fn normalize_git_status_truncates_over_limit() {
        let mut status = String::from("## main...origin/main\n");
        while status.len() <= GIT_STATUS_CHARACTER_LIMIT {
            status.push_str(" M src/some/long/path/to/file.rs\n");
        }
        assert!(status.len() > GIT_STATUS_CHARACTER_LIMIT);
        let out = normalize_git_status(&status).expect("non-empty status");
        assert!(
            out.ends_with("\n\n... (git status truncated)"),
            "missing truncation marker: {out}"
        );
        let body = out
            .strip_suffix("\n\n... (git status truncated)")
            .expect("marker suffix");
        assert!(
            body.len() <= GIT_STATUS_CHARACTER_LIMIT,
            "body {} exceeds cap {GIT_STATUS_CHARACTER_LIMIT}",
            body.len()
        );
        assert!(status.starts_with(body), "body is not a clean prefix");
        assert!(!body.ends_with('\n'), "body should be snapped to last line");
    }
}
