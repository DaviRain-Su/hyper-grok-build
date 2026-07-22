use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

pub struct WorkflowsCommand;

impl SlashCommand for WorkflowsCommand {
    fn name(&self) -> &str {
        "workflows"
    }

    fn description(&self) -> &str {
        "Show workflow runs (phases, agents, progress)"
    }

    fn usage(&self) -> &str {
        "/workflows"
    }

    fn visible(&self, _ctx: &crate::slash::command::AppCtx) -> bool {
        true
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::ToggleWorkflows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::bundle::BundleState;
    use crate::settings::PagerLocalSnapshot;

    static DEFAULT_BUNDLE_STATE: BundleState = BundleState {
        has_cache: false,
        version: String::new(),
        personas: Vec::new(),
        roles: Vec::new(),
        agents: Vec::new(),
        skills: Vec::new(),
        persona_details: Vec::new(),
        role_details: Vec::new(),
    };

    #[test]
    fn visibility_is_defensive_during_catalog_reload() {
        let models = ModelState::default();
        for available in [false, true] {
            let ctx = crate::slash::command::AppCtx {
                models: &models,
                cwd: std::path::Path::new("."),
                has_session_announcements: false,
                billing_surface_visible: true,
                workflows_available: available,
                screen_mode: crate::app::ScreenMode::Fullscreen,
            };
            assert!(WorkflowsCommand.visible(&ctx));
        }
    }

    #[test]
    fn dispatches_toggle_workflows() {
        let models = ModelState::default();
        let mut ctx = CommandExecCtx {
            models: &models,
            session_id: None,
            bundle_state: &DEFAULT_BUNDLE_STATE,
            screen_mode: crate::app::ScreenMode::Minimal,
            billing_surface_visible: true,
            pager_state: PagerLocalSnapshot::default(),
        };
        assert!(matches!(
            WorkflowsCommand.run(&mut ctx, ""),
            CommandResult::Action(Action::ToggleWorkflows)
        ));
    }
}
