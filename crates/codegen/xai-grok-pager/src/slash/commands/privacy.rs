//! `/privacy` -- show or toggle privacy and data retention status.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

/// Show or toggle privacy and data retention status.
///
/// Usage:
/// - `/privacy`             show current status
/// - `/privacy opt-in`      opt in to coding data sharing
/// - `/privacy opt-out`     opt out of coding data sharing
///
/// Case-insensitive. Only unambiguous aliases are accepted (e.g. `in`,
/// `share`, `out`, `private`) — generic toggles like `on`/`off` are
/// rejected because they're ambiguous in privacy context.
pub struct PrivacyCommand;

impl SlashCommand for PrivacyCommand {
    fn name(&self) -> &str {
        "privacy"
    }

    fn description(&self) -> &str {
        "Show or toggle privacy & data retention status"
    }

    fn usage(&self) -> &str {
        "/privacy [opt-in|opt-out]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let arg = args.trim();
        if arg.is_empty() {
            return CommandResult::Action(Action::ShowPrivacyInfo);
        }
        match parse_privacy_arg(arg) {
            Some(opted_in) => CommandResult::Action(Action::SetCodingDataSharing { opted_in }),
            None => CommandResult::Error(format!(
                "Unknown argument `{arg}`. Valid options: opt-in (aliases: in, share) | \
                 opt-out (aliases: out, private)."
            )),
        }
    }
}

/// Parse `/privacy <arg>` into `Some(true)` (opt-in), `Some(false)`
/// (opt-out), or `None` (unknown). Case-insensitive ASCII matching.
#[doc(hidden)]
pub fn parse_privacy_arg(arg: &str) -> Option<bool> {
    const OPT_IN_ALIASES: &[&str] = &["opt-in", "in", "share"];
    const OPT_OUT_ALIASES: &[&str] = &["opt-out", "out", "private"];

    if OPT_IN_ALIASES.iter().any(|a| arg.eq_ignore_ascii_case(a)) {
        return Some(true);
    }
    if OPT_OUT_ALIASES.iter().any(|a| arg.eq_ignore_ascii_case(a)) {
        return Some(false);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_opt_in_canonical() {
        assert_eq!(parse_privacy_arg("opt-in"), Some(true));
    }

    #[test]
    fn parse_opt_out_canonical() {
        assert_eq!(parse_privacy_arg("opt-out"), Some(false));
    }

    /// Case-insensitive matching.
    #[test]
    fn parse_case_insensitive() {
        for variant in &["OPT-IN", "Opt-In", "opt-IN", "OpT-iN"] {
            assert_eq!(
                parse_privacy_arg(variant),
                Some(true),
                "case-insensitive parse must accept `{variant}` as opt-in",
            );
        }
        for variant in &["OPT-OUT", "Opt-Out", "opt-OUT", "OpT-oUt"] {
            assert_eq!(
                parse_privacy_arg(variant),
                Some(false),
                "case-insensitive parse must accept `{variant}` as opt-out",
            );
        }
    }

    /// Pins the accepted alias catalog.
    #[test]
    fn parse_opt_in_aliases() {
        for alias in &["in", "share"] {
            assert_eq!(
                parse_privacy_arg(alias),
                Some(true),
                "alias `{alias}` must map to opt-in",
            );
        }
    }

    #[test]
    fn parse_opt_out_aliases() {
        for alias in &["out", "private"] {
            assert_eq!(
                parse_privacy_arg(alias),
                Some(false),
                "alias `{alias}` must map to opt-out",
            );
        }
    }

    /// Ambiguous generic-toggle aliases must be rejected — `/privacy on`
    /// is ambiguous (could mean opt-in or opt-out).
    #[test]
    fn parse_rejects_ambiguous_generic_aliases() {
        for ambiguous in &[
            "on", "off", "true", "false", "enable", "enabled", "disable", "disabled",
        ] {
            assert_eq!(
                parse_privacy_arg(ambiguous),
                None,
                "ambiguous alias `{ambiguous}` MUST be rejected — it would let a user typing \
                 `/privacy {ambiguous}` get the OPPOSITE of their intent in privacy context. \
                 See Security Issue 10 in PR 9 R1.",
            );
        }
    }

    /// Unknown arguments return None → the command surfaces an error
    /// listing valid options. Pins the "no silent fallback" contract.
    #[test]
    fn parse_unknown_returns_none() {
        for unknown in &["yes", "no", "maybe", "opt-maybe", "", " ", "1", "0"] {
            assert_eq!(
                parse_privacy_arg(unknown),
                None,
                "unknown arg `{unknown}` must NOT parse",
            );
        }
    }

    /// Alias families must not overlap.
    #[test]
    fn alias_families_disjoint() {
        let opt_in_results: Vec<bool> = ["opt-in", "in", "share"]
            .iter()
            .map(|a| parse_privacy_arg(a).unwrap())
            .collect();
        assert!(
            opt_in_results.iter().all(|b| *b),
            "every opt-in alias must parse to true",
        );
        let opt_out_results: Vec<bool> = ["opt-out", "out", "private"]
            .iter()
            .map(|a| parse_privacy_arg(a).unwrap())
            .collect();
        assert!(
            opt_out_results.iter().all(|b| !*b),
            "every opt-out alias must parse to false",
        );
    }

    /// Error message must list every accepted alias.
    #[test]
    fn error_message_lists_all_accepted_aliases() {
        use crate::acp::model_state::ModelState;
        use crate::app::bundle::BundleState;

        let cmd = PrivacyCommand;
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut ctx = CommandExecCtx {
            models: &models,
            session_id: None,
            bundle_state: &bundle,
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot::default(),
        };
        let result = cmd.run(&mut ctx, "garbage-input");
        match result {
            CommandResult::Error(msg) => {
                // Every accepted alias appears in the error message.
                for alias in &["opt-in", "in", "share", "opt-out", "out", "private"] {
                    assert!(
                        msg.contains(alias),
                        "error message must mention alias `{alias}` so the user knows \
                         what to type; msg = {msg:?}",
                    );
                }
                // Dropped ambiguous aliases must not appear.
                for dropped in &["off", "true", "false", "enable", "disable"] {
                    assert!(
                        !msg.contains(dropped),
                        "dropped alias `{dropped}` must NOT appear in error message \
                         (would suggest it's still accepted); msg = {msg:?}",
                    );
                }
            }
            other => panic!("expected Error result for unknown arg, got {other:?}"),
        }
    }
}
