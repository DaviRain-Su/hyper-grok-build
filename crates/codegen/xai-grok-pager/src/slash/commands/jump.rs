use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

pub struct JumpCommand;

impl SlashCommand for JumpCommand {
    fn name(&self) -> &str {
        "jump"
    }

    fn description(&self) -> &str {
        "Jump to a turn in the conversation"
    }

    fn session_scoped(&self) -> bool {
        true
    }

    /// Minimal mode has no interactive scrollback pane to scroll — the
    /// terminal's own scrollback covers it (same gate as `/find`).
    fn available_in_minimal(&self) -> bool {
        false
    }

    fn usage(&self) -> &str {
        "/jump"
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::JumpShowPicker)
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
    fn jump_returns_show_picker_action() {
        let models = ModelState::default();
        let mut ctx = CommandExecCtx {
            models: &models,
            session_id: None,
            bundle_state: &DEFAULT_BUNDLE_STATE,
            screen_mode: crate::app::ScreenMode::Fullscreen,
            billing_surface_visible: true,
            pager_state: PagerLocalSnapshot::default(),
        };
        let result = JumpCommand.run(&mut ctx, "");
        assert!(matches!(
            result,
            CommandResult::Action(Action::JumpShowPicker)
        ));
    }

    #[test]
    fn not_available_in_minimal() {
        // Native terminal scrollback replaces in-app scrolling in minimal.
        assert!(!JumpCommand.available_in_minimal());
    }
}
