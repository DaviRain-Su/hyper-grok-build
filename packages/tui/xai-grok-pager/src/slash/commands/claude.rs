//! `/claude` — short alias for `/login claude`.
//!
//! Sign in with an Anthropic Claude Pro/Max subscription (browser OAuth) and
//! unlock the `anthropic-claude/*` models. The credential persists under the
//! `oauth/anthropic-claude` scope in `~/.grok/auth.json` and auto-refreshes, so
//! this is a one-time login.
//!
//! - `/claude`        — start (or re-run) the subscription login
//! - `/claude logout` — points to `hyper logout --claude` (CLI-side logout)

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

pub struct ClaudeCommand;

impl SlashCommand for ClaudeCommand {
    fn name(&self) -> &str {
        "claude"
    }

    fn aliases(&self) -> &[&str] {
        &["anthropic"]
    }

    fn description(&self) -> &str {
        "Log in with your Claude Pro/Max subscription (short for /login claude)"
    }

    fn usage(&self) -> &str {
        "/claude"
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        match args.trim().to_ascii_lowercase().as_str() {
            // Login is the whole point of the shortcut.
            "" | "login" | "signin" | "sign-in" => {
                CommandResult::Action(Action::LoginAnthropicClaude)
            }
            // Claude logout is a CLI-side operation (no in-TUI action yet).
            "logout" | "off" | "signout" | "sign-out" => CommandResult::Error(
                "To sign out of Claude, run `hyper logout --claude` in your shell \
                 (or `hyper logout --all`)."
                    .into(),
            ),
            other => CommandResult::Error(format!(
                "Unknown option '{other}'. Use `/claude` to sign in, or `hyper logout --claude` to sign out."
            )),
        }
    }
}
