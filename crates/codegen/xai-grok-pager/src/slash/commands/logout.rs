//! `/logout` — remove auth credentials.
//!
//! - bare `/logout` — full xAI logout (return to login screen)
//! - `/logout provider <platform>` — clear a third-party API key stored via
//!   `/providers` (alias of `/providers clear <platform>`)

use crate::app::actions::Action;
use crate::slash::command::{AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand};

pub struct LogoutCommand;

impl SlashCommand for LogoutCommand {
    fn name(&self) -> &str {
        "logout"
    }

    fn description(&self) -> &str {
        "Log out (xAI session, or a platform API key)"
    }

    fn usage(&self) -> &str {
        "/logout [provider <platform>]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn args_required(&self) -> bool {
        false
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("[provider <platform>]")
    }

    fn suggest_args(&self, _ctx: &AppCtx, args_query: &str) -> Option<Vec<ArgItem>> {
        let trimmed = args_query.trim_start();
        if trimmed.is_empty() {
            return Some(vec![ArgItem::new(
                "provider  (clear a /providers API key)",
                "provider platform byok",
                "provider ",
                "Then pick a platform id, e.g. zai-coding",
            )]);
        }
        let (first, rest) = split_first(trimmed);
        if matches!(first, "provider" | "platform" | "byok") && rest.is_empty() {
            // List API-key platforms for second token.
            let items = xai_grok_models::PlatformId::ALL
                .into_iter()
                .filter(|p| !p.uses_oauth())
                .map(|p| {
                    ArgItem::new(
                        format!("{}  {}", p.as_str(), p.display_name()),
                        p.as_str(),
                        p.as_str(),
                        "Clear stored API key from auth.json",
                    )
                })
                .collect();
            return Some(items);
        }
        None
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        if trimmed.is_empty() {
            return CommandResult::Action(Action::Logout);
        }

        let (first, rest) = split_first(trimmed);
        if matches!(
            first.to_ascii_lowercase().as_str(),
            "provider" | "platform" | "byok"
        ) {
            let platform_tok = rest.trim();
            if platform_tok.is_empty() {
                return CommandResult::Error(
                    "Usage: /logout provider <platform>\n\
                     Example: /logout provider zai-coding\n\
                     (same as /providers clear zai-coding)"
                        .into(),
                );
            }
            let (plat, _) = split_first(platform_tok);
            let Some(platform) = xai_grok_models::PlatformId::parse(plat) else {
                return CommandResult::Error(format!(
                    "Unknown platform '{plat}'. Try /providers clear and pick one."
                ));
            };
            if platform.uses_oauth() {
                return CommandResult::Error(format!(
                    "{} uses OAuth — run `grok logout --kimi` instead.",
                    platform.display_name()
                ));
            }
            return CommandResult::Action(Action::SetPlatformApiKey {
                platform: platform.as_str().to_owned(),
                api_key: String::new(),
            });
        }

        CommandResult::Error(format!(
            "Unknown /logout args '{trimmed}'.\n\
             /logout                  — sign out of xAI\n\
             /logout provider <id>    — clear a platform API key"
        ))
    }
}

fn split_first(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.split_once(char::is_whitespace) {
        Some((a, b)) => (a, b.trim_start()),
        None => (s, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::bundle::BundleState;
    use crate::settings::PagerLocalSnapshot;
    use crate::acp::model_state::ModelState;

    static EMPTY_BUNDLE: BundleState = BundleState {
        has_cache: false,
        version: String::new(),
        personas: Vec::new(),
        roles: Vec::new(),
        agents: Vec::new(),
        skills: Vec::new(),
        persona_details: Vec::new(),
        role_details: Vec::new(),
    };

    fn ctx(models: &ModelState) -> CommandExecCtx<'_> {
        CommandExecCtx {
            models,
            session_id: None,
            bundle_state: &EMPTY_BUNDLE,
            screen_mode: crate::app::ScreenMode::Inline,
            pager_state: PagerLocalSnapshot::default(),
        }
    }

    #[test]
    fn bare_logout_dispatches_action() {
        let models = ModelState::default();
        let mut c = ctx(&models);
        match LogoutCommand.run(&mut c, "") {
            CommandResult::Action(Action::Logout) => {}
            other => panic!("expected Logout, got {other:?}"),
        }
    }

    #[test]
    fn logout_provider_clears_platform_key() {
        let models = ModelState::default();
        let mut c = ctx(&models);
        match LogoutCommand.run(&mut c, "provider zai-coding") {
            CommandResult::Action(Action::SetPlatformApiKey { platform, api_key }) => {
                assert_eq!(platform, "zai-coding");
                assert!(api_key.is_empty());
            }
            other => panic!("expected SetPlatformApiKey clear, got {other:?}"),
        }
    }
}
