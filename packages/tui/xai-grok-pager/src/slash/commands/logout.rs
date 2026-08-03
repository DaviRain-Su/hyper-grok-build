//! `/logout` — remove auth credentials.
//!
//! - bare `/logout` — full xAI logout (return to login screen)
//! - `/logout kimi|openai|claude|github|radius` — clear only that provider's OAuth scope
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
        "Log out (xAI, provider OAuth, or a platform API key)"
    }

    fn usage(&self) -> &str {
        "/logout [kimi|openai|claude|github|radius|provider <platform>]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn args_required(&self) -> bool {
        false
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("[kimi|openai|claude|github|radius|provider <platform>]")
    }

    fn suggest_args(&self, _ctx: &AppCtx, args_query: &str) -> Option<Vec<ArgItem>> {
        let trimmed = args_query.trim_start();
        if trimmed.is_empty() {
            return Some(vec![
                ArgItem::new(
                    "kimi  (clear Kimi Code OAuth)",
                    "kimi kimi-code oauth",
                    "kimi",
                    "Keep any static Kimi API key",
                ),
                ArgItem::new(
                    "openai  (clear OpenAI Codex OAuth)",
                    "openai codex chatgpt oauth",
                    "openai",
                    "Keep the xAI session",
                ),
                ArgItem::new(
                    "claude  (clear Anthropic Claude OAuth)",
                    "claude anthropic oauth",
                    "claude",
                    "Keep the xAI session",
                ),
                ArgItem::new(
                    "github  (clear GitHub Copilot OAuth)",
                    "github copilot oauth",
                    "github",
                    "Keep any static Copilot token",
                ),
                ArgItem::new(
                    "radius  (clear Radius OAuth)",
                    "radius oauth",
                    "radius",
                    "Keep any static Radius API key",
                ),
                ArgItem::new(
                    "provider  (clear a /providers API key)",
                    "provider platform byok",
                    "provider ",
                    "Then pick a platform id, e.g. zai-coding",
                ),
            ]);
        }
        let (first, rest) = split_first(trimmed);
        if matches!(first, "provider" | "platform" | "byok") && rest.is_empty() {
            // List API-key platforms for second token.
            let items = xai_grok_models::provider_registry()
                .providers()
                .iter()
                .filter(|provider| provider.accepts_api_key())
                .map(|provider| {
                    ArgItem::new(
                        format!("{}  {}", provider.id, provider.display_name),
                        provider.id.as_str(),
                        provider.id.as_str(),
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
            "kimi" | "kimi-code" | "kimi-coding"
        ) {
            return CommandResult::Action(Action::LogoutKimi);
        }
        if matches!(
            first.to_ascii_lowercase().as_str(),
            "openai" | "openai-codex" | "codex" | "chatgpt"
        ) {
            return CommandResult::Action(Action::LogoutOpenAiCodex);
        }
        if matches!(
            first.to_ascii_lowercase().as_str(),
            "claude" | "anthropic" | "anthropic-claude"
        ) {
            return CommandResult::Action(Action::LogoutAnthropicClaude);
        }
        if matches!(
            first.to_ascii_lowercase().as_str(),
            "github" | "github-copilot" | "copilot"
        ) {
            return CommandResult::Action(Action::LogoutGitHubCopilot);
        }
        if first.eq_ignore_ascii_case("radius") {
            return CommandResult::Action(Action::LogoutRadius);
        }
        if matches!(
            first.to_ascii_lowercase().as_str(),
            "amazon-bedrock" | "bedrock"
        ) {
            return CommandResult::Action(Action::SetPlatformApiKey {
                platform: "amazon-bedrock".to_string(),
                api_key: String::new(),
                base_url: None,
            });
        }
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
            let Some(provider) = xai_grok_models::provider_spec(plat) else {
                return CommandResult::Error(format!(
                    "Unknown provider '{plat}'. Try /providers clear and pick one."
                ));
            };
            if !provider.accepts_api_key() {
                let logout = provider
                    .legacy_platform()
                    .map(super::oauth_login_logout_hint)
                    .map(|(_, logout)| logout)
                    .unwrap_or("/logout");
                return CommandResult::Error(format!(
                    "{} uses OAuth — run `{logout}` instead.",
                    provider.display_name
                ));
            }
            return CommandResult::Action(Action::SetPlatformApiKey {
                platform: provider.id.as_str().to_owned(),
                api_key: String::new(),
                base_url: None,
            });
        }

        CommandResult::Error(format!(
            "Unknown /logout args '{trimmed}'.\n\
             /logout                  — sign out of xAI\n\
             /logout kimi             — clear Kimi Code OAuth only\n\
             /logout openai           — clear OpenAI Codex OAuth only\n\
             /logout claude           — clear Anthropic Claude OAuth only\n\
             /logout github           — clear GitHub Copilot OAuth only\n\
             /logout radius           — clear Radius OAuth only\n\
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
    use crate::acp::model_state::ModelState;
    use crate::app::bundle::BundleState;
    use crate::settings::PagerLocalSnapshot;

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
            billing_surface_visible: true,
            usage_command_visible: true,
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
    fn logout_routes_all_subscription_oauth_scopes() {
        let models = ModelState::default();
        let mut c = ctx(&models);
        assert!(matches!(
            LogoutCommand.run(&mut c, "kimi"),
            CommandResult::Action(Action::LogoutKimi)
        ));
        assert!(matches!(
            LogoutCommand.run(&mut c, "codex"),
            CommandResult::Action(Action::LogoutOpenAiCodex)
        ));
        assert!(matches!(
            LogoutCommand.run(&mut c, "anthropic"),
            CommandResult::Action(Action::LogoutAnthropicClaude)
        ));
        assert!(matches!(
            LogoutCommand.run(&mut c, "copilot"),
            CommandResult::Action(Action::LogoutGitHubCopilot)
        ));
        assert!(matches!(
            LogoutCommand.run(&mut c, "radius"),
            CommandResult::Action(Action::LogoutRadius)
        ));
    }

    #[test]
    fn logout_provider_clears_platform_key() {
        let models = ModelState::default();
        let mut c = ctx(&models);
        match LogoutCommand.run(&mut c, "provider zai-coding") {
            CommandResult::Action(Action::SetPlatformApiKey {
                platform, api_key, ..
            }) => {
                assert_eq!(platform, "zai-coding");
                assert!(api_key.is_empty());
            }
            other => panic!("expected SetPlatformApiKey clear, got {other:?}"),
        }
    }

    #[test]
    fn logout_provider_openai_codex_shows_openai_hint() {
        let models = ModelState::default();
        let mut c = ctx(&models);
        match LogoutCommand.run(&mut c, "provider openai-codex") {
            CommandResult::Error(msg) => {
                assert!(
                    msg.contains("grok logout --openai"),
                    "Codex logout hint must point at --openai, got: {msg}"
                );
                assert!(
                    !msg.contains("--kimi"),
                    "Codex logout hint must not mention --kimi, got: {msg}"
                );
            }
            other => panic!("expected Error for OAuth platform, got {other:?}"),
        }
    }

    #[test]
    fn logout_provider_chatgpt_codex_alias_shows_openai_hint() {
        let models = ModelState::default();
        let mut c = ctx(&models);
        match LogoutCommand.run(&mut c, "provider chatgpt-codex") {
            CommandResult::Error(msg) => {
                assert!(
                    msg.contains("grok logout --openai"),
                    "chatgpt-codex alias must also hint --openai, got: {msg}"
                );
            }
            other => panic!("expected Error for OAuth platform, got {other:?}"),
        }
    }

    #[test]
    fn logout_provider_kimi_code_clears_only_static_key_for_hybrid_provider() {
        let models = ModelState::default();
        let mut c = ctx(&models);
        match LogoutCommand.run(&mut c, "provider kimi-code") {
            CommandResult::Action(Action::SetPlatformApiKey {
                platform, api_key, ..
            }) => {
                assert_eq!(platform, "kimi-code");
                assert!(api_key.is_empty());
            }
            other => panic!("expected static API-key clear for hybrid Kimi, got {other:?}"),
        }
    }
}
