//! `/delete` — delete this session's history and return home.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

pub struct DeleteCommand;

impl SlashCommand for DeleteCommand {
    fn name(&self) -> &str {
        "delete"
    }

    fn description(&self) -> &str {
        "Delete this session and return home"
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn usage(&self) -> &str {
        "/delete"
    }

    fn run(&self, ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        if ctx.session_id.is_none() {
            return CommandResult::Error("No active session to delete".into());
        }
        CommandResult::Action(Action::DeleteCurrentSession)
    }
}
