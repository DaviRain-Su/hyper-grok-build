//! `/edit-prompt` -- edit the minimal-mode composer in an external editor.

use crate::app::actions::Action;
use crate::slash::command::{AppCtx, CommandExecCtx, CommandResult, SlashCommand};

/// Minimal-only fallback for terminals that reserve `Ctrl+G`.
pub struct EditPromptCommand;

impl SlashCommand for EditPromptCommand {
    fn name(&self) -> &str {
        "edit-prompt"
    }

    fn description(&self) -> &str {
        "Open an external editor for an empty prompt; use the command palette to preserve a draft"
    }

    fn usage(&self) -> &str {
        "/edit-prompt"
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn visible(&self, ctx: &AppCtx) -> bool {
        ctx.screen_mode.is_minimal()
    }

    fn run(&self, ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        if !ctx.screen_mode.is_minimal() {
            return CommandResult::Error(
                "/edit-prompt is only available in minimal mode".to_owned(),
            );
        }
        if ctx.session_id.is_none() {
            return CommandResult::Error("No active session".to_owned());
        }
        CommandResult::Action(Action::EditPromptExternal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::bundle::BundleState;
    use crate::settings::PagerLocalSnapshot;

    fn app_ctx<'a>(models: &'a ModelState, mode: crate::app::ScreenMode) -> AppCtx<'a> {
        AppCtx {
            models,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            screen_mode: mode,
            workflows_available: true,
        }
    }

    fn exec_ctx<'a>(
        models: &'a ModelState,
        bundle: &'a BundleState,
        session_id: Option<&'a agent_client_protocol::SessionId>,
        mode: crate::app::ScreenMode,
    ) -> CommandExecCtx<'a> {
        CommandExecCtx {
            models,
            session_id,
            bundle_state: bundle,
            screen_mode: mode,
            billing_surface_visible: true,
            pager_state: PagerLocalSnapshot::default(),
        }
    }

    #[test]
    fn visible_and_executable_only_in_minimal() {
        let command = EditPromptCommand;
        let models = ModelState::default();
        let bundle = BundleState::default();
        let session_id = agent_client_protocol::SessionId::from("session".to_owned());

        assert!(command.visible(&app_ctx(&models, crate::app::ScreenMode::Minimal)));
        assert!(!command.visible(&app_ctx(&models, crate::app::ScreenMode::Fullscreen)));
        assert!(matches!(
            command.run(
                &mut exec_ctx(
                    &models,
                    &bundle,
                    Some(&session_id),
                    crate::app::ScreenMode::Minimal,
                ),
                "",
            ),
            CommandResult::Action(Action::EditPromptExternal)
        ));
        assert!(matches!(
            command.run(
                &mut exec_ctx(
                    &models,
                    &bundle,
                    Some(&session_id),
                    crate::app::ScreenMode::Fullscreen,
                ),
                "",
            ),
            CommandResult::Error(message) if message.contains("only available in minimal mode")
        ));
    }

    #[test]
    fn requires_session() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        assert!(matches!(
            EditPromptCommand.run(
                &mut exec_ctx(
                    &models,
                    &bundle,
                    None,
                    crate::app::ScreenMode::Minimal,
                ),
                "",
            ),
            CommandResult::Error(message) if message.contains("No active session")
        ));
    }
}
