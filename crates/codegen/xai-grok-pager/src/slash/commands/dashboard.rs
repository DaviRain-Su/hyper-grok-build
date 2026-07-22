//! `/dashboard` — open the Agent Dashboard view.
//!
//! Centralised overview of every running session (top-level + subagents)
//! with peek, attach, and dispatch from one screen. The dashboard reuses
//! the existing fullscreen subagent takeover for "attach to subagent",
//! so attaching never bypasses `active_subagent`.
//!
//! Same `Action`-only run path as other session-less commands, no args.
//! `/sessions` is an alias (see [`SlashCommand::aliases`]): the dashboard
//! replaced the removed sessions picker modal for switching, renaming, and
//! closing active sessions. Visibility is feature-flag-gated: the
//! command is hidden by default in the registry and revealed when the
//! dashboard feature is enabled (`dashboard_enabled()`), via
//! [`crate::app::agent_view::AgentView::set_dashboard_visible`]. When
//! `[dashboard].enabled = false` or `GROK_AGENT_DASHBOARD=0` is set, the
//! dispatcher prints a friendly toast and refuses to open. The dashboard is
//! independent of leader mode.

use crate::app::actions::Action;
use crate::slash::command::{AppCtx, CommandExecCtx, CommandResult, SlashCommand};

/// Open the Agent Dashboard view.
pub struct DashboardCommand;

impl SlashCommand for DashboardCommand {
    fn name(&self) -> &str {
        "dashboard"
    }

    /// `/agents-dashboard` is registered as an alias. The canonical
    /// name remains `/dashboard`.
    ///
    /// `/sessions` survives the sessions-modal removal as an alias: the
    /// dashboard is the replacement surface for switching, renaming, and
    /// closing active sessions, so old muscle memory redirects here. As an
    /// alias it inherits the feature-flag gate (`set_dashboard_visible`
    /// hides by canonical name) and the minimal-mode gates below.
    fn aliases(&self) -> &[&str] {
        &["agents-dashboard", "sessions"]
    }

    fn description(&self) -> &str {
        "Open the Agent Dashboard — a fullscreen overview of every running session"
    }

    fn usage(&self) -> &str {
        "/dashboard"
    }

    /// The agent dashboard is intentionally out of scope in minimal mode
    /// (single-session standalone — K14/§6.15). Gated off with a message.
    fn available_in_minimal(&self) -> bool {
        false
    }

    /// Hidden from the completion dropdown in minimal mode: the dashboard
    /// (and its `/sessions` / `/agents-dashboard` spellings) has nothing to
    /// open there, so offering it just to refuse at dispatch is noise. A
    /// fully-typed invocation still resolves and hits the central
    /// `available_in_minimal` dispatch gate (friendly refusal, fail-closed).
    fn visible(&self, ctx: &AppCtx) -> bool {
        !ctx.screen_mode.is_minimal()
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::OpenDashboard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::bundle::BundleState;
    use crate::slash::command::{AppCtx, CommandExecCtx, CommandResult};

    #[test]
    fn run_returns_open_dashboard_action() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut ctx = CommandExecCtx {
            models: &models,
            session_id: None,
            bundle_state: &bundle,
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot {
                multiline_mode: false,
                yolo_mode: false,
                ..crate::settings::PagerLocalSnapshot::default()
            },
        };
        let cmd = DashboardCommand;
        assert!(matches!(
            cmd.run(&mut ctx, ""),
            CommandResult::Action(Action::OpenDashboard)
        ));
    }

    /// Feature-flag gating is applied externally by the registry
    /// (`set_dashboard_visible`), not via `visible()` — `AppCtx` carries no
    /// dashboard state. `visible()` only gates on screen mode: offered in
    /// fullscreen/inline, hidden from the minimal-mode dropdown (where the
    /// dashboard has nothing to open and dispatch would just refuse).
    #[test]
    fn visible_everywhere_except_minimal() {
        let models = ModelState::default();
        let cmd = DashboardCommand;
        let ctx = |screen_mode| AppCtx {
            models: &models,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            workflows_available: true,
            screen_mode,
        };
        assert!(cmd.visible(&ctx(crate::app::ScreenMode::Fullscreen)));
        assert!(cmd.visible(&ctx(crate::app::ScreenMode::Inline)));
        assert!(
            !cmd.visible(&ctx(crate::app::ScreenMode::Minimal)),
            "the dashboard (and its /sessions alias) must not be offered in minimal mode"
        );
    }

    #[test]
    fn does_not_take_args() {
        let cmd = DashboardCommand;
        assert!(!cmd.takes_args());
    }

    #[test]
    fn name_is_dashboard() {
        let cmd = DashboardCommand;
        assert_eq!(cmd.name(), "dashboard");
    }

    /// `/sessions` (removed picker modal) and `/agents-dashboard`
    /// both spell this command.
    #[test]
    fn aliases_include_sessions() {
        let cmd = DashboardCommand;
        assert_eq!(cmd.aliases(), &["agents-dashboard", "sessions"]);
    }

    #[test]
    fn not_available_in_minimal() {
        // The dashboard is out of scope in scrollback-native minimal mode.
        assert!(!DashboardCommand.available_in_minimal());
    }
}
