//! `/changes` — review and accept/reject pending agent edits (A2 diff review).

use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

const USAGE: &str = "Usage: /changes";

pub struct ChangesCommand;

impl SlashCommand for ChangesCommand {
    fn name(&self) -> &str {
        "changes"
    }

    fn aliases(&self) -> &[&str] {
        &["review"]
    }

    fn description(&self) -> &str {
        "Review pending edits and accept or reject them"
    }

    fn usage(&self) -> &str {
        "/changes"
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        if args.trim().is_empty() {
            CommandResult::Changes
        } else {
            CommandResult::Error(USAGE.to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::bundle::BundleState;

    fn run(args: &str) -> CommandResult {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut context = CommandExecCtx {
            models: &models,
            session_id: None,
            bundle_state: &bundle,
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot::default(),
        };
        ChangesCommand.run(&mut context, args)
    }

    #[test]
    fn bare_changes_dispatches_review() {
        assert!(matches!(run(""), CommandResult::Changes));
        assert!(matches!(run("  "), CommandResult::Changes));
    }

    #[test]
    fn rejects_arguments() {
        assert!(matches!(run("all"), CommandResult::Error(message) if message.contains(USAGE)));
    }
}
