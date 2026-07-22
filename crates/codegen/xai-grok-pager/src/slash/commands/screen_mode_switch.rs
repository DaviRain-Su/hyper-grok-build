//! `/minimal` and `/fullscreen` — session-scoped re-exec of the active session.

use crate::app::ScreenMode;
use crate::app::actions::Action;
use crate::slash::command::{AppCtx, CommandExecCtx, CommandResult, SlashCommand};

/// Reopen the active session in the other screen mode (`/minimal` ⇄ `/fullscreen`).
pub struct ScreenModeSwitchCommand {
    /// `true` → `/minimal` (fullscreen → scrollback-native);
    /// `false` → `/fullscreen` (minimal → alt-screen TUI).
    to_minimal: bool,
}

impl ScreenModeSwitchCommand {
    /// `/minimal`: offered in fullscreen, relaunches with `--minimal`.
    pub const fn minimal() -> Self {
        Self { to_minimal: true }
    }

    /// `/fullscreen` (alias `/full`): offered in minimal, relaunches without
    /// `--minimal`.
    pub const fn fullscreen() -> Self {
        Self { to_minimal: false }
    }

    /// The mode this command switches *away from* — the only mode it is
    /// offered in (switching to the mode you are already in is meaningless).
    fn source_mode_active(&self, mode: ScreenMode) -> bool {
        if self.to_minimal {
            mode.is_fullscreen()
        } else {
            mode.is_minimal()
        }
    }

    fn target_label(&self) -> &'static str {
        if self.to_minimal {
            "minimal"
        } else {
            "fullscreen"
        }
    }

    fn source_label(&self) -> &'static str {
        if self.to_minimal {
            "fullscreen"
        } else {
            "minimal"
        }
    }
}

impl SlashCommand for ScreenModeSwitchCommand {
    fn name(&self) -> &str {
        self.target_label()
    }

    fn aliases(&self) -> &[&str] {
        if self.to_minimal { &[] } else { &["full"] }
    }

    fn description(&self) -> &str {
        if self.to_minimal {
            "Reopen this session in minimal (scrollback-native) mode — switch back with /fullscreen"
        } else {
            "Reopen this session in fullscreen mode — switch back with /minimal"
        }
    }

    fn usage(&self) -> &str {
        if self.to_minimal {
            "/minimal"
        } else {
            "/fullscreen"
        }
    }

    fn session_scoped(&self) -> bool {
        true
    }

    /// `/minimal` switches *away from* fullscreen, so it is pointless inside
    /// minimal; `/fullscreen` is the way back out.
    fn available_in_minimal(&self) -> bool {
        !self.to_minimal
    }

    /// Only offered while the mode being switched away from is active.
    fn visible(&self, ctx: &AppCtx) -> bool {
        self.source_mode_active(ctx.screen_mode)
    }

    fn run(&self, ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        if !self.source_mode_active(ctx.screen_mode) {
            return CommandResult::Error(format!(
                "/{} is only available in {} mode",
                self.target_label(),
                self.source_label(),
            ));
        }
        if ctx.session_id.is_none() {
            return CommandResult::Error(format!(
                "No active session to reopen in {} mode",
                self.target_label(),
            ));
        }
        CommandResult::Action(Action::RelaunchInScreenMode {
            minimal: self.to_minimal,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::bundle::BundleState;

    fn app_ctx<'a>(models: &'a ModelState, mode: ScreenMode) -> AppCtx<'a> {
        AppCtx {
            models,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            workflows_available: true,
            screen_mode: mode,
        }
    }

    fn exec_ctx<'a>(
        models: &'a ModelState,
        bundle: &'a BundleState,
        mode: ScreenMode,
        session: Option<&'a agent_client_protocol::SessionId>,
    ) -> CommandExecCtx<'a> {
        CommandExecCtx {
            models,
            session_id: session,
            bundle_state: bundle,
            screen_mode: mode,
            billing_surface_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot::default(),
        }
    }

    #[test]
    fn minimal_visible_only_in_fullscreen() {
        let models = ModelState::default();
        let cmd = ScreenModeSwitchCommand::minimal();
        assert!(cmd.visible(&app_ctx(&models, ScreenMode::Fullscreen)));
        assert!(!cmd.visible(&app_ctx(&models, ScreenMode::Minimal)));
        assert!(!cmd.visible(&app_ctx(&models, ScreenMode::Inline)));
    }

    #[test]
    fn fullscreen_visible_only_in_minimal() {
        let models = ModelState::default();
        let cmd = ScreenModeSwitchCommand::fullscreen();
        assert!(cmd.visible(&app_ctx(&models, ScreenMode::Minimal)));
        assert!(!cmd.visible(&app_ctx(&models, ScreenMode::Fullscreen)));
        assert!(!cmd.visible(&app_ctx(&models, ScreenMode::Inline)));
    }

    #[test]
    fn run_returns_relaunch_action_with_session() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let sid = agent_client_protocol::SessionId::from("sess-abc".to_string());

        let mut ctx = exec_ctx(&models, &bundle, ScreenMode::Fullscreen, Some(&sid));
        assert!(matches!(
            ScreenModeSwitchCommand::minimal().run(&mut ctx, ""),
            CommandResult::Action(Action::RelaunchInScreenMode { minimal: true })
        ));

        let mut ctx = exec_ctx(&models, &bundle, ScreenMode::Minimal, Some(&sid));
        assert!(matches!(
            ScreenModeSwitchCommand::fullscreen().run(&mut ctx, ""),
            CommandResult::Action(Action::RelaunchInScreenMode { minimal: false })
        ));
    }

    #[test]
    fn run_errors_without_session() {
        let models = ModelState::default();
        let bundle = BundleState::default();

        let mut ctx = exec_ctx(&models, &bundle, ScreenMode::Fullscreen, None);
        assert!(matches!(
            ScreenModeSwitchCommand::minimal().run(&mut ctx, ""),
            CommandResult::Error(msg) if msg.contains("No active session")
        ));

        let mut ctx = exec_ctx(&models, &bundle, ScreenMode::Minimal, None);
        assert!(matches!(
            ScreenModeSwitchCommand::fullscreen().run(&mut ctx, ""),
            CommandResult::Error(msg) if msg.contains("No active session")
        ));
    }

    #[test]
    fn run_errors_outside_source_mode() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let sid = agent_client_protocol::SessionId::from("sess-abc".to_string());

        // `/minimal` outside fullscreen.
        let mut ctx = exec_ctx(&models, &bundle, ScreenMode::Inline, Some(&sid));
        assert!(matches!(
            ScreenModeSwitchCommand::minimal().run(&mut ctx, ""),
            CommandResult::Error(msg) if msg.contains("fullscreen")
        ));

        // `/fullscreen` outside minimal.
        let mut ctx = exec_ctx(&models, &bundle, ScreenMode::Fullscreen, Some(&sid));
        assert!(matches!(
            ScreenModeSwitchCommand::fullscreen().run(&mut ctx, ""),
            CommandResult::Error(msg) if msg.contains("minimal mode")
        ));
    }

    #[test]
    fn minimal_availability_mirrors_direction() {
        // `/minimal` is a fullscreen-pane switcher; `/fullscreen` is the way
        // back out of minimal.
        assert!(!ScreenModeSwitchCommand::minimal().available_in_minimal());
        assert!(ScreenModeSwitchCommand::fullscreen().available_in_minimal());
    }
}
