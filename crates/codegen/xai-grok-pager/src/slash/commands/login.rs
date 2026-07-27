//! `/login` -- log in or re-authenticate with your account.
//!
//! Optional argument: `kimi` / `kimi-code` starts Kimi Code device OAuth;
//! `openai` / `codex` starts the OpenAI Codex (ChatGPT) browser OAuth;
//! `claude` / `anthropic` starts the Anthropic Claude subscription OAuth.
//! OpenCode Go uses a Console-issued API key rather than a portable OAuth
//! login, so `/login opencode-go` redirects to `/providers opencode-go`.
//! With no argument, login always uses the default xAI flow.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

pub struct LoginCommand;

impl SlashCommand for LoginCommand {
    fn name(&self) -> &str {
        "login"
    }

    fn description(&self) -> &str {
        "Log in or show subscription setup (kimi | openai | claude | opencode-go)"
    }

    fn usage(&self) -> &str {
        "/login [kimi|openai|claude|opencode-go]"
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let arg = args.trim().to_ascii_lowercase();
        if matches!(arg.as_str(), "kimi" | "kimi-code") {
            CommandResult::Action(Action::LoginKimi)
        } else if matches!(
            arg.as_str(),
            "openai" | "openai-codex" | "codex" | "chatgpt"
        ) {
            CommandResult::Action(Action::LoginOpenAiCodex)
        } else if matches!(arg.as_str(), "claude" | "anthropic" | "anthropic-claude") {
            CommandResult::Action(Action::LoginAnthropicClaude)
        } else if matches!(arg.as_str(), "opencode-go" | "opencodego") {
            CommandResult::Error(
                "OpenCode Go subscriptions use a Console-issued API key, not portable OAuth. \
                 Subscribe at https://opencode.ai/go, then run \
                 `/providers opencode-go <api_key>` (or set OPENCODE_API_KEY)."
                    .into(),
            )
        } else if matches!(arg.as_str(), "nexus" | "providers" | "provider") {
            // Nexus is BYOK (API key), not OAuth — redirect instead of erroring.
            CommandResult::Error(
                "Nexus 用 API key,不走 OAuth —— 请用 `/nexus <key>`(TUI 内)或 \
                 `hyper nexus <key>`(命令行,登录前即可用)。空敲 `/nexus` 查看引导。"
                    .into(),
            )
        } else if arg.is_empty() {
            CommandResult::Action(Action::Login)
        } else {
            CommandResult::Error(format!(
                "Unknown login target '{arg}'. Try `/login`, `/login kimi`, `/login openai`, \
                 `/login claude`, or `/login opencode-go` (API-key setup); for Nexus use `/nexus`."
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;

    static EMPTY_BUNDLE: crate::app::bundle::BundleState = crate::app::bundle::BundleState {
        has_cache: false,
        version: String::new(),
        personas: Vec::new(),
        roles: Vec::new(),
        agents: Vec::new(),
        skills: Vec::new(),
        persona_details: Vec::new(),
        role_details: Vec::new(),
    };

    fn dummy_exec_ctx(models: &ModelState) -> CommandExecCtx<'_> {
        CommandExecCtx {
            models,
            session_id: None,
            bundle_state: &EMPTY_BUNDLE,
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot::default(),
        }
    }

    #[test]
    fn login_nexus_redirects_to_nexus_command() {
        let models = ModelState::default();
        let mut ctx = dummy_exec_ctx(&models);
        match LoginCommand.run(&mut ctx, "nexus") {
            CommandResult::Error(msg) => assert!(msg.contains("/nexus"), "msg: {msg}"),
            other => panic!("expected redirect Error, got {other:?}"),
        }
    }

    #[test]
    fn login_opencode_go_redirects_to_official_api_key_flow() {
        let models = ModelState::default();
        let mut ctx = dummy_exec_ctx(&models);
        assert!(LoginCommand.description().contains("opencode-go"));
        assert!(LoginCommand.usage().contains("opencode-go"));
        match LoginCommand.run(&mut ctx, "opencode-go") {
            CommandResult::Error(message) => {
                assert!(message.contains("/providers opencode-go <api_key>"));
                assert!(message.contains("OPENCODE_API_KEY"));
                assert!(message.contains("https://opencode.ai/go"));
            }
            other => panic!("expected API-key redirect, got {other:?}"),
        }
    }

    #[test]
    fn login_routes_all_four_interactive_families() {
        let models = ModelState::default();
        let mut ctx = dummy_exec_ctx(&models);

        assert!(matches!(
            LoginCommand.run(&mut ctx, ""),
            CommandResult::Action(Action::Login)
        ));
        assert!(matches!(
            LoginCommand.run(&mut ctx, "kimi"),
            CommandResult::Action(Action::LoginKimi)
        ));
        assert!(matches!(
            LoginCommand.run(&mut ctx, "openai"),
            CommandResult::Action(Action::LoginOpenAiCodex)
        ));
        assert!(matches!(
            LoginCommand.run(&mut ctx, "claude"),
            CommandResult::Action(Action::LoginAnthropicClaude)
        ));
    }
}
