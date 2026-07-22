//! `/doctor` — diagnose terminal, color/theme, clipboard, and voice input.
//!
//! Runs the shared TUI probe and diagnostics path, including live runtime
//! evidence that the standalone command cannot observe.

use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

pub struct DoctorCommand;

impl SlashCommand for DoctorCommand {
    fn name(&self) -> &str {
        "doctor"
    }

    fn aliases(&self) -> &[&str] {
        &["terminal-setup", "terminal-check", "terminal-info"]
    }

    fn description(&self) -> &str {
        "Check terminal, color, clipboard, and voice input"
    }

    fn usage(&self) -> &str {
        "/doctor"
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn run(&self, ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        let terminal = crate::terminal::terminal_context();
        let query = crate::diagnostics::probes::LiveTmuxProbe;
        let snapshot = crate::diagnostics::probes::collect_doctor_tui(
            terminal,
            crate::diagnostics::probes::TuiProbeEvidence {
                fullscreen_active: ctx.screen_mode.is_fullscreen(),
                kitty_flags_pushed: crate::app::kitty_flags_pushed(),
                xtversion: crate::terminal::xtversion::detected(),
            },
            &query,
        );
        let mut report = crate::diagnostics::view(snapshot.into());
        // Passive enumeration cannot detect a denied macOS grant; capture reports that separately.
        if crate::app::voice_mode_enabled() {
            crate::diagnostics::apply_voice_probe(&mut report, true);
        }
        CommandResult::Message(crate::diagnostics::format_doctor(&report))
    }
}
