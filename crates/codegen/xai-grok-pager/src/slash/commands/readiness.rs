//! `/readiness` — assess whether the workspace is ready for agent autonomy.
//!
//! Repo-focused counterpart to `/doctor` (which checks the terminal): probes
//! AGENTS.md, build/test/CI/lint infrastructure, git health, and lockfiles.
//! See `docs/competitive-analysis.md` A6.

use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

const USAGE: &str = "Usage: /readiness";

pub struct ReadinessCommand;

impl SlashCommand for ReadinessCommand {
    fn name(&self) -> &str {
        "readiness"
    }

    fn aliases(&self) -> &[&str] {
        &["repo-check"]
    }

    fn description(&self) -> &str {
        "Check whether this repo is ready for agent autonomy"
    }

    fn usage(&self) -> &str {
        "/readiness"
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        if args.trim().is_empty() {
            CommandResult::Readiness
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
        ReadinessCommand.run(&mut context, args)
    }

    #[test]
    fn bare_readiness_dispatches_report() {
        assert!(matches!(run(""), CommandResult::Readiness));
        assert!(matches!(run("   "), CommandResult::Readiness));
    }

    #[test]
    fn rejects_arguments() {
        assert!(matches!(run("now"), CommandResult::Error(message) if message.contains(USAGE)));
    }
}
