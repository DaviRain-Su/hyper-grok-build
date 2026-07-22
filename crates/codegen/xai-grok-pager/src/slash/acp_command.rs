//! Wrapper that turns an ACP `AvailableCommand` into a `SlashCommand`.
//!
//! ACP-advertised commands appear in the dropdown but pass through to the
//! shell for execution. The wrapper stores `String` fields -- consistent
//! with the `&str` trait design.
//!
//! Skill commands (those with `meta.path` + `meta.scope`) are handled
//! client-side: pager reads the SKILL.md, applies substitutions, and
//! sends structured prompt blocks directly. Non-skill ACP commands
//! pass through to the shell as before.

use agent_client_protocol as acp;
use xai_grok_tools::implementations::skills::types::SkillScope;

use super::command::{CommandExecCtx, CommandResult, SlashCommand};

/// A slash command backed by an ACP `AvailableCommand`.
///
/// For skill commands (has `skill_path` + `skill_scope`), execution reads
/// the SKILL.md client-side and produces `CommandResult::InjectSkill`.
/// For non-skill commands, execution produces `CommandResult::PassThrough`.
pub struct AcpSlashCommand {
    name: String,
    description: String,
    has_args: bool,
    arg_hint: Option<String>,
    /// Skill-specific: path to SKILL.md on disk. None for shell builtins.
    skill_path: Option<String>,
    /// Skill-specific: parsed scope enum. None for shell builtins.
    skill_scope: Option<SkillScope>,
    /// True if the ACP meta had skill-like keys but they were invalid.
    meta_malformed: bool,
}

impl SlashCommand for AcpSlashCommand {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn usage(&self) -> &str {
        &self.name
    }

    fn takes_args(&self) -> bool {
        self.has_args
    }

    /// ACP commands always accept Enter -- args are never required locally.
    /// The shell validates.
    fn args_required(&self) -> bool {
        false
    }

    fn arg_placeholder(&self) -> Option<&str> {
        self.arg_hint.as_deref()
    }

    fn is_skill(&self) -> bool {
        self.skill_path.is_some() && self.skill_scope.is_some()
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        // Malformed skill metadata — surface error, don't silently degrade.
        if self.meta_malformed {
            return CommandResult::Error(format!("Malformed skill metadata for /{}", self.name));
        }

        // Non-skill ACP commands: pass through to the shell as before.
        if self.skill_path.is_none() || self.skill_scope.is_none() {
            let text = if args.trim().is_empty() {
                format!("/{}", self.name)
            } else {
                format!("/{} {}", self.name, args)
            };
            return CommandResult::PassThrough(text);
        }

        // --- Pass skill through to the shell for expansion ---
        //
        // The shell's slash_commands::resolve() handles skill detection,
        // SKILL.md loading, substitution, and assembly of the
        // <user_query> + <skill_information> format. The pager just
        // sends the raw `/skill args` text as a single prompt block.
        let display_text = if args.trim().is_empty() {
            format!("/{}", self.name)
        } else {
            format!("/{} {}", self.name, args)
        };

        let prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(
            display_text.clone(),
        ))];

        CommandResult::InjectSkill {
            display_text,
            prompt_blocks,
            display_as_skill: true,
            scheduled_task_preview: None,
        }
    }
}

impl From<&acp::AvailableCommand> for AcpSlashCommand {
    fn from(cmd: &acp::AvailableCommand) -> Self {
        let arg_hint = cmd.input.as_ref().and_then(|input| match input {
            acp::AvailableCommandInput::Unstructured(u) => Some(u.hint.clone()),
            // TODO(acp-0.10): `AvailableCommandInput` is #[non_exhaustive].
            _ => None,
        });

        // Parse skill metadata from ACP `_meta`:
        //   { "scope": "local", "path": "/path/to/SKILL.md" }
        //
        // Missing meta → non-skill ACP command (PassThrough).
        // Present but malformed meta → meta_malformed = true (Error on run()).
        let (skill_path, skill_scope, meta_malformed) = match cmd.meta.as_ref() {
            None => (None, None, false),
            Some(m) => {
                let path = m
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let scope: Option<SkillScope> = m
                    .get("scope")
                    .and_then(|v| serde_json::from_value(v.clone()).ok());
                if path.is_some() && scope.is_some() {
                    (path, scope, false)
                } else if m.get("scope").is_some() && scope.is_none() {
                    (None, None, false)
                } else if m.get("path").is_some() || m.get("scope").is_some() {
                    (None, None, true)
                } else {
                    // Meta exists but has no skill keys (e.g., other metadata)
                    (None, None, false)
                }
            }
        };

        Self {
            name: cmd.name.clone(),
            description: cmd.description.clone(),
            // ACP commands always accept free-form input. The shell handles
            // whatever text follows the command name. The `input` field only
            // determines the placeholder hint, not whether args are allowed.
            has_args: true,
            arg_hint,
            skill_path,
            skill_scope,
            meta_malformed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cmd(name: &str, meta: Option<serde_json::Value>) -> acp::AvailableCommand {
        let mut cmd = acp::AvailableCommand::new(name.to_string(), format!("{name} command"));
        if let Some(m) = meta.and_then(|v| v.as_object().cloned()) {
            cmd = cmd.meta(m);
        }
        cmd
    }

    #[test]
    fn no_meta_is_non_skill() {
        let cmd = make_cmd("flush", None);
        let acp_cmd = AcpSlashCommand::from(&cmd);
        assert!(acp_cmd.skill_path.is_none());
        assert!(acp_cmd.skill_scope.is_none());
        assert!(!acp_cmd.meta_malformed);
    }

    #[test]
    fn unknown_scope_passes_through_instead_of_erroring() {
        let cmd = make_cmd(
            "pr-cleanup",
            Some(serde_json::json!({
                "scope": "workflow",
                "path": ".grok/workflows/pr-cleanup.rhai",
            })),
        );
        let acp_cmd = AcpSlashCommand::from(&cmd);
        assert!(!acp_cmd.meta_malformed);
        assert!(!acp_cmd.is_skill());

        let models = crate::acp::model_state::ModelState::default();
        let mut ctx = CommandExecCtx {
            models: &models,
            session_id: None,
            bundle_state: &crate::app::bundle::BundleState::default(),
            screen_mode: crate::app::ScreenMode::Minimal,
            billing_surface_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot::default(),
        };
        match acp_cmd.run(&mut ctx, "fix the branch") {
            CommandResult::PassThrough(text) => {
                assert_eq!(text, "/pr-cleanup fix the branch");
            }
            other => panic!("expected PassThrough, got {other:?}"),
        }
    }

    #[test]
    fn valid_skill_meta_populates_fields() {
        let meta = serde_json::json!({
            "scope": "local",
            "path": "/home/user/.grok/skills/commit/SKILL.md"
        });
        let cmd = make_cmd("commit", Some(meta));
        let acp_cmd = AcpSlashCommand::from(&cmd);
        assert_eq!(
            acp_cmd.skill_path.as_deref(),
            Some("/home/user/.grok/skills/commit/SKILL.md")
        );
        assert_eq!(acp_cmd.skill_scope, Some(SkillScope::Local));
        assert!(!acp_cmd.meta_malformed);
    }

    #[test]
    fn unknown_scope_value_is_foreign_kind_not_malformed() {
        let meta = serde_json::json!({
            "scope": "invalid_scope",
            "path": "/path/to/SKILL.md"
        });
        let cmd = make_cmd("broken", Some(meta));
        let acp_cmd = AcpSlashCommand::from(&cmd);
        assert!(acp_cmd.skill_path.is_none());
        assert!(acp_cmd.skill_scope.is_none());
        assert!(!acp_cmd.meta_malformed);
    }

    #[test]
    fn path_not_string_is_malformed() {
        let meta = serde_json::json!({
            "scope": "local",
            "path": 42
        });
        let cmd = make_cmd("broken", Some(meta));
        let acp_cmd = AcpSlashCommand::from(&cmd);
        assert!(acp_cmd.meta_malformed);
    }

    #[test]
    fn scope_only_no_path_is_malformed() {
        let meta = serde_json::json!({
            "scope": "user"
        });
        let cmd = make_cmd("partial", Some(meta));
        let acp_cmd = AcpSlashCommand::from(&cmd);
        assert!(acp_cmd.meta_malformed);
    }

    #[test]
    fn path_only_no_scope_is_malformed() {
        let meta = serde_json::json!({
            "path": "/path/to/SKILL.md"
        });
        let cmd = make_cmd("partial", Some(meta));
        let acp_cmd = AcpSlashCommand::from(&cmd);
        assert!(acp_cmd.meta_malformed);
    }

    #[test]
    fn unrelated_meta_is_non_skill() {
        let meta = serde_json::json!({
            "foo": "bar",
            "baz": 42
        });
        let cmd = make_cmd("other", Some(meta));
        let acp_cmd = AcpSlashCommand::from(&cmd);
        assert!(acp_cmd.skill_path.is_none());
        assert!(acp_cmd.skill_scope.is_none());
        assert!(!acp_cmd.meta_malformed);
    }

    #[test]
    fn all_scope_variants_parse_correctly() {
        for (scope_str, expected) in [
            ("local", SkillScope::Local),
            ("repo", SkillScope::Repo),
            ("user", SkillScope::User),
            ("plugin", SkillScope::Plugin),
        ] {
            let meta = serde_json::json!({
                "scope": scope_str,
                "path": "/path/to/SKILL.md"
            });
            let cmd = make_cmd("test", Some(meta));
            let acp_cmd = AcpSlashCommand::from(&cmd);
            assert_eq!(acp_cmd.skill_scope, Some(expected), "scope={scope_str}");
            assert!(!acp_cmd.meta_malformed);
        }
    }

    // -- run() tests --

    fn make_skill_cmd(name: &str, path: &str, scope: &str) -> AcpSlashCommand {
        AcpSlashCommand {
            name: name.to_string(),
            description: format!("{name} skill"),
            has_args: true,
            arg_hint: None,
            skill_path: Some(path.to_string()),
            skill_scope: serde_json::from_value(serde_json::json!(scope)).ok(),
            meta_malformed: false,
        }
    }

    fn make_exec_ctx() -> CommandExecCtx<'static> {
        use crate::acp::model_state::ModelState;
        let models = Box::leak(Box::new(ModelState::default()));
        let bundle = Box::leak(Box::new(crate::app::bundle::BundleState::default()));
        CommandExecCtx {
            models,
            session_id: None,
            bundle_state: bundle,
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot {
                multiline_mode: false,
                yolo_mode: false,
                ..crate::settings::PagerLocalSnapshot::default()
            },
        }
    }

    #[test]
    fn run_non_skill_passes_through() {
        let cmd = AcpSlashCommand {
            name: "flush".to_string(),
            description: "flush".to_string(),
            has_args: true,
            arg_hint: None,
            skill_path: None,
            skill_scope: None,
            meta_malformed: false,
        };
        let mut ctx = make_exec_ctx();
        let result = cmd.run(&mut ctx, "");
        assert!(matches!(result, CommandResult::PassThrough(t) if t == "/flush"));
    }

    #[test]
    fn run_malformed_meta_returns_error() {
        let cmd = AcpSlashCommand {
            name: "broken".to_string(),
            description: "broken".to_string(),
            has_args: true,
            arg_hint: None,
            skill_path: None,
            skill_scope: None,
            meta_malformed: true,
        };
        let mut ctx = make_exec_ctx();
        let result = cmd.run(&mut ctx, "");
        assert!(matches!(result, CommandResult::Error(msg) if msg.contains("Malformed")));
    }

    #[test]
    fn run_missing_file_passes_through_to_shell() {
        // The pager no longer reads SKILL.md — it passes through to the shell.
        // A missing file still produces InjectSkill with the raw `/skill args` text.
        let cmd = make_skill_cmd("commit", "/nonexistent/path/SKILL.md", "local");
        let mut ctx = make_exec_ctx();
        let result = cmd.run(&mut ctx, "fix bug");
        match result {
            CommandResult::InjectSkill {
                display_text,
                prompt_blocks,
                ..
            } => {
                assert_eq!(display_text, "/commit fix bug");
                assert_eq!(prompt_blocks.len(), 1);
                let text = match &prompt_blocks[0] {
                    acp::ContentBlock::Text(t) => &t.text,
                    other => panic!("expected Text, got {:?}", other),
                };
                assert_eq!(text, "/commit fix bug");
            }
            other => panic!("expected InjectSkill, got {:?}", other),
        }
    }

    #[test]
    fn run_skill_produces_inject_skill() {
        // The pager sends raw `/skill args` text — the shell handles expansion.
        let cmd = make_skill_cmd("commit", "/some/path/SKILL.md", "local");
        let mut ctx = make_exec_ctx();
        let result = cmd.run(&mut ctx, "fix the auth bug");

        match result {
            CommandResult::InjectSkill {
                display_text,
                prompt_blocks,
                ..
            } => {
                assert_eq!(display_text, "/commit fix the auth bug");
                // Single block with the raw `/skill args` text.
                assert_eq!(prompt_blocks.len(), 1);
                let text = match &prompt_blocks[0] {
                    acp::ContentBlock::Text(t) => &t.text,
                    other => panic!("expected Text, got {:?}", other),
                };
                assert_eq!(text, "/commit fix the auth bug");
                // No XML markup.
                assert!(!text.contains("<command-name>"));
                assert!(!text.contains("<skill"));
            }
            other => panic!("expected InjectSkill, got {:?}", other),
        }
    }

    #[test]
    fn run_skill_no_args_omits_command_args_tag() {
        let cmd = make_skill_cmd("deploy", "/some/path/SKILL.md", "user");
        let mut ctx = make_exec_ctx();
        let result = cmd.run(&mut ctx, "");

        match result {
            CommandResult::InjectSkill {
                display_text,
                prompt_blocks,
                ..
            } => {
                assert_eq!(display_text, "/deploy");
                assert_eq!(prompt_blocks.len(), 1);
                let text = match &prompt_blocks[0] {
                    acp::ContentBlock::Text(t) => &t.text,
                    other => panic!("expected Text, got {:?}", other),
                };
                assert_eq!(text, "/deploy");
            }
            other => panic!("expected InjectSkill, got {:?}", other),
        }
    }

    #[test]
    fn run_skill_qualified_name_not_double_prefixed() {
        // Shell advertises "local:commit" when there's a duplicate bare name.
        let cmd = make_skill_cmd("local:commit", "/some/path/SKILL.md", "local");
        let mut ctx = make_exec_ctx();
        let result = cmd.run(&mut ctx, "fix bug");

        match result {
            CommandResult::InjectSkill {
                display_text,
                prompt_blocks,
                ..
            } => {
                assert_eq!(display_text, "/local:commit fix bug");
                assert_eq!(prompt_blocks.len(), 1);
                let text = match &prompt_blocks[0] {
                    acp::ContentBlock::Text(t) => &t.text,
                    other => panic!("expected Text, got {:?}", other),
                };
                assert_eq!(text, "/local:commit fix bug");
            }
            other => panic!("expected InjectSkill, got {:?}", other),
        }
    }

    #[test]
    fn run_skill_user_qualified_name_preserved() {
        // Shell advertises "user:commit" for a user-scoped skill that collides.
        let cmd = make_skill_cmd("user:commit", "/some/path/SKILL.md", "user");
        let mut ctx = make_exec_ctx();
        let result = cmd.run(&mut ctx, "");

        match result {
            CommandResult::InjectSkill {
                display_text,
                prompt_blocks,
                ..
            } => {
                assert_eq!(display_text, "/user:commit");
                assert_eq!(prompt_blocks.len(), 1);
                let text = match &prompt_blocks[0] {
                    acp::ContentBlock::Text(t) => &t.text,
                    other => panic!("expected Text, got {:?}", other),
                };
                assert_eq!(text, "/user:commit");
            }
            other => panic!("expected InjectSkill, got {:?}", other),
        }
    }

    #[test]
    fn run_skill_builtin_colliding_name_preserved() {
        // Shell advertises "local:compact" when a skill collides with the
        // built-in /compact command.
        let cmd = make_skill_cmd("local:compact", "/some/path/SKILL.md", "local");
        let mut ctx = make_exec_ctx();
        let result = cmd.run(&mut ctx, "");

        match result {
            CommandResult::InjectSkill {
                display_text,
                prompt_blocks,
                ..
            } => {
                assert_eq!(display_text, "/local:compact");
                assert_eq!(prompt_blocks.len(), 1);
                let text = match &prompt_blocks[0] {
                    acp::ContentBlock::Text(t) => &t.text,
                    other => panic!("expected Text, got {:?}", other),
                };
                assert_eq!(text, "/local:compact");
            }
            other => panic!("expected InjectSkill, got {:?}", other),
        }
    }

    #[test]
    fn run_skill_substitutes_skill_dir() {
        // The pager no longer does substitutions — it passes through to the shell.
        // This test verifies the pass-through behavior.
        let cmd = make_skill_cmd("config", "/some/path/SKILL.md", "local");
        let mut ctx = make_exec_ctx();
        let result = cmd.run(&mut ctx, "");

        match result {
            CommandResult::InjectSkill {
                display_text,
                prompt_blocks,
                ..
            } => {
                assert_eq!(display_text, "/config");
                assert_eq!(prompt_blocks.len(), 1);
                let text = match &prompt_blocks[0] {
                    acp::ContentBlock::Text(t) => &t.text,
                    other => panic!("expected Text, got {:?}", other),
                };
                assert_eq!(text, "/config");
            }
            other => panic!("expected InjectSkill, got {:?}", other),
        }
    }
}
