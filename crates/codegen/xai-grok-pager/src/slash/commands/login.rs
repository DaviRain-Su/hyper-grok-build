//! `/login` -- log in or re-authenticate with your account.
//!
//! Optional argument: `kimi` / `kimi-code` starts Kimi Code device OAuth;
//! `openai` / `codex` starts the OpenAI Codex (ChatGPT) browser OAuth
//! instead of the default xAI login.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

pub struct LoginCommand;

impl SlashCommand for LoginCommand {
    fn name(&self) -> &str {
        "login"
    }

    fn description(&self) -> &str {
        "Log in or re-authenticate (optional: kimi | openai)"
    }

    fn usage(&self) -> &str {
        "/login [kimi|openai]"
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let arg = args.trim().to_ascii_lowercase();
        if matches!(arg.as_str(), "kimi" | "kimi-code") {
            CommandResult::Action(Action::LoginKimi)
        } else if matches!(
            arg.as_str(),
            "openai" | "openai-codex" | "codex" | "chatgpt"
        ) {
            CommandResult::Action(Action::LoginOpenAiCodex)
        } else if arg.is_empty() {
            CommandResult::Action(Action::Login)
        } else {
            CommandResult::Error(format!(
                "Unknown login target '{arg}'. Try `/login`, `/login kimi` or `/login openai`."
            ))
        }
    }
}
