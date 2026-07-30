//! `/edit-prompt` -- edit the minimal-mode composer in an external editor.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};
use crate::slash::{ModeSupport, Remedy};

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

    fn mode_support(&self) -> ModeSupport {
        ModeSupport::MinimalOnly(Remedy::SwitchMode {
            why: "the full TUI has no external-editor path — Ctrl+G is the tasks pane there",
        })
    }

    fn run(&self, ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
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
            usage_command_visible: true,
            pager_state: PagerLocalSnapshot::default(),
        }
    }

    #[test]
    fn opens_the_external_editor() {
        let command = EditPromptCommand;
        let models = ModelState::default();
        let bundle = BundleState::default();
        let session_id = agent_client_protocol::SessionId::from("session".to_owned());

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
