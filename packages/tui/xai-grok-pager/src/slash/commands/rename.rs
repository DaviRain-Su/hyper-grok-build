//! `/rename` (alias `/title`) -- rename the current session.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

/// Rename the current session's title/summary.
pub struct RenameCommand;

impl SlashCommand for RenameCommand {
    fn name(&self) -> &str {
        "rename"
    }

    fn aliases(&self) -> &[&str] {
        &["title"]
    }

    fn description(&self) -> &str {
        "Rename the current session"
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn usage(&self) -> &str {
        "/rename <title>"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn args_required(&self) -> bool {
        true
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("<title>")
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        if ctx.session_id.is_none() {
            return CommandResult::Error("No active session".to_string());
        }

        let title = args.trim().to_string();
        if title.is_empty() {
            return CommandResult::Error("Usage: /rename <new title>".to_string());
        }

        CommandResult::Action(Action::RenameSession { title })
    }
}
