//! `/quit` (alias `/exit`) -- quit the application.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

/// Quit the pager application.
pub struct ExitCommand;

impl SlashCommand for ExitCommand {
    fn name(&self) -> &str {
        "quit"
    }

    fn aliases(&self) -> &[&str] {
        &["exit"]
    }

    fn description(&self) -> &str {
        "Quit the application"
    }

    fn usage(&self) -> &str {
        "/quit"
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::Quit)
    }
}
