//! `/feedback`: send session feedback.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

/// Bare `/feedback` opens the freeform report pane; `/feedback <text>` submits immediately.
pub struct FeedbackCommand;

impl SlashCommand for FeedbackCommand {
    fn name(&self) -> &str {
        "feedback"
    }

    fn description(&self) -> &str {
        "Send feedback about the current session"
    }

    fn usage(&self) -> &str {
        "/feedback [text]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("[feedback text]")
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        if trimmed.is_empty() {
            CommandResult::Action(Action::OpenFeedbackPane)
        } else {
            CommandResult::Action(Action::SendFeedback(trimmed.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;

    fn make_ctx(models: &ModelState) -> CommandExecCtx<'_> {
        let bundle = Box::leak(Box::new(crate::app::bundle::BundleState::default()));
        CommandExecCtx {
            models,
            session_id: None,
            bundle_state: bundle,
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            usage_command_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot::default(),
        }
    }

    /// The whitespace case matters: the composer keeps a trailing space while the user is still typing the command.
    #[test]
    fn bare_and_whitespace_only_open_the_pane_while_text_submits() {
        let models = ModelState::default();
        let mut ctx = make_ctx(&models);
        let cmd = FeedbackCommand;

        for args in ["", "   ", "\t"] {
            match cmd.run(&mut ctx, args) {
                CommandResult::Action(Action::OpenFeedbackPane) => {}
                other => panic!("{args:?} should open the pane, got {other:?}"),
            }
        }

        match cmd.run(&mut ctx, "  the tool crashed  ") {
            CommandResult::Action(Action::SendFeedback(text)) => {
                assert_eq!(text, "the tool crashed", "inline text is trimmed");
            }
            other => panic!("inline text should submit, got {other:?}"),
        }
    }
}
