//! `/doctor` — diagnose terminal, color/theme, clipboard, and voice input.
//!
//! Runs the shared TUI probe and diagnostics path, including live runtime
//! evidence that the standalone command cannot observe.

use crate::slash::command::{
    AppCtx, ArgItem, CommandExecCtx, CommandResult, DoctorRequest, SlashCommand,
};

const USAGE: &str = "Usage: /doctor [fix [ssh-wrap]]";

pub struct DoctorCommand;

impl DoctorCommand {
    pub(crate) fn report(
        screen_mode: crate::app::ScreenMode,
        runtime: crate::diagnostics::TuiRuntimeRequest<'_>,
    ) -> crate::diagnostics::DiagnosticReport {
        let terminal = crate::terminal::terminal_context();
        let query = crate::diagnostics::probes::LiveTmuxProbe;
        let snapshot = crate::diagnostics::probes::collect_doctor_tui(
            terminal,
            crate::diagnostics::probes::TuiProbeEvidence {
                fullscreen_active: screen_mode.is_fullscreen(),
                kitty_flags_pushed: crate::app::kitty_flags_pushed(),
                xtversion: crate::terminal::xtversion::detected(),
            },
            &query,
        );
        let runtime_findings = crate::diagnostics::collect_tui_runtime_findings(
            &snapshot.common,
            runtime.notification_method,
            runtime.notification_protocol,
            runtime.notification_condition,
            runtime.workspace,
        );
        let mut report = crate::diagnostics::view(snapshot.into());
        crate::diagnostics::merge_tui_runtime_findings(&mut report, runtime_findings);
        report
    }
}

impl SlashCommand for DoctorCommand {
    fn name(&self) -> &str {
        "doctor"
    }

    fn aliases(&self) -> &[&str] {
        &["terminal-setup", "terminal-check", "terminal-info"]
    }

    fn description(&self) -> &str {
        "Check this session and show available fixes"
    }

    fn usage(&self) -> &str {
        "/doctor [fix [ssh-wrap]]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("[fix [ssh-wrap]]")
    }

    fn suggest_args(&self, _ctx: &AppCtx, args_query: &str) -> Option<Vec<ArgItem>> {
        let query = args_query.trim();
        if query.is_empty() || matches!(query, "fix ssh-wrap" | "fix terminal.ssh-wrap") {
            return None;
        }
        let item = if query == "fix" || query.starts_with("fix ") {
            ArgItem {
                display: "ssh-wrap".into(),
                match_text: "fix ssh-wrap terminal.ssh-wrap".into(),
                insert_text: "fix ssh-wrap".into(),
                description: "Set up SSH wrapping on this computer".into(),
                locked: false,
                action_id: None,
                hidden: false,
            }
        } else {
            ArgItem {
                display: "fix".into(),
                match_text: "fix".into(),
                insert_text: "fix".into(),
                description: "Show automatic fixes available here".into(),
                locked: false,
                action_id: None,
                hidden: false,
            }
        };
        Some(vec![item])
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let mut tokens = args.split_whitespace();
        match (tokens.next(), tokens.next(), tokens.next()) {
            (None, None, None) => CommandResult::Doctor(DoctorRequest::Report),
            (Some("fix"), None, None) => CommandResult::Doctor(DoctorRequest::ListFixes),
            (Some("fix"), Some(value), None) => match crate::diagnostics::resolve_fix_id(value) {
                Ok(id) => CommandResult::Doctor(DoctorRequest::Fix(id)),
                Err(error) => CommandResult::Error(format!("{error}\n{USAGE}")),
            },
            _ => CommandResult::Error(USAGE.to_owned()),
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
        DoctorCommand.run(&mut context, args)
    }

    #[test]
    fn parses_report_list_short_and_canonical_fix_forms() {
        assert!(matches!(
            run(""),
            CommandResult::Doctor(DoctorRequest::Report)
        ));
        assert!(matches!(
            run("fix"),
            CommandResult::Doctor(DoctorRequest::ListFixes)
        ));
        for value in ["ssh-wrap", "terminal.ssh-wrap"] {
            assert!(matches!(
                run(&format!("fix {value}")),
                CommandResult::Doctor(DoctorRequest::Fix(crate::diagnostics::SSH_WRAP_ID))
            ));
        }
    }

    #[test]
    fn rejects_unknown_and_extra_arguments() {
        for value in ["unknown", "fix unknown", "fix ssh-wrap extra", "report now"] {
            assert!(matches!(run(value), CommandResult::Error(message) if message.contains(USAGE)));
        }
    }

    #[test]
    fn completion_stays_closed_until_an_argument_starts() {
        let models = ModelState::default();
        let context = AppCtx {
            models: &models,
            cwd: std::path::Path::new("/tmp"),
            has_session_announcements: false,
            billing_surface_visible: true,
            workflows_available: false,
            screen_mode: crate::app::ScreenMode::Inline,
        };
        let command = DoctorCommand;
        assert!(command.suggest_args(&context, "").is_none());
        assert!(command.suggest_args(&context, "   ").is_none());
        assert_eq!(
            command.suggest_args(&context, "f").unwrap()[0].insert_text,
            "fix"
        );
        for query in ["fix", "fix ", "fix s", "fix ssh", "fix terminal."] {
            assert_eq!(
                command.suggest_args(&context, query).unwrap()[0].insert_text,
                "fix ssh-wrap"
            );
        }
        for query in [
            "fix ssh-wrap",
            " fix ssh-wrap ",
            "fix terminal.ssh-wrap",
            "  fix terminal.ssh-wrap  ",
        ] {
            assert!(command.suggest_args(&context, query).is_none(), "{query:?}");
        }
    }
}
