//! `/nexus` — short alias for `/providers nexus`.
//!
//! Set the Nexus relay key once and it persists to `~/.grok/auth.json`
//! (scope `platform/nexus`), auto-loaded on the next launch:
//!
//! - `/nexus <api_key>`            — save the key (default gateway root)
//! - `/nexus <api_key> <base_url>` — save the key + a custom gateway root
//! - `/nexus clear`               — remove the stored key (alias: off / logout)
//!
//! A bare `/nexus` (no args) prints setup guidance instead of doing nothing —
//! see [`nexus_setup_guide`], which is shared with the `hyper nexus` CLI
//! subcommand so the two entry points never drift.
//!
//! Everything else is delegated to [`super::providers::ProvidersCommand`] with a
//! `nexus ` prefix, so parsing, persistence, and error hints stay in one place.

use xai_grok_models::NEXUS_BASE_URL_DEFAULT;

use crate::slash::command::{AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand};

use super::providers::ProvidersCommand;

/// Onboarding text shown by bare `/nexus` and by `hyper nexus` (no args).
///
/// Shared between the TUI slash command and the `hyper nexus` CLI subcommand
/// (`xai-grok-pager-bin`) so the guidance stays identical in both places.
pub fn nexus_setup_guide() -> String {
    format!(
        "Nexus 中转站 —— 一个 API key 解锁 OpenAI + Claude(Messages/Responses),\n\
         模型随后以 nexus/* 出现在模型选择器里。\n\
         \n\
         1) 申请 API key:  {NEXUS_BASE_URL_DEFAULT}\n\
         2) 保存(只需一次,持久化到 ~/.grok/auth.json):\n\
         \x20    hyper nexus <api_key>                    # 命令行,登录前即可用\n\
         \x20    /nexus <api_key>                         # 在 TUI 内\n\
         \x20    /nexus <api_key> https://your-gateway    # 自定义网关根(可选)\n\
         3) 删除:  hyper nexus --clear   或   /nexus clear"
    )
}

/// The compiled-in default Nexus gateway root (`https://nexuscore.now`).
///
/// Re-exported so the `hyper nexus` CLI subcommand can show it without adding a
/// direct `xai-grok-models` dependency to the binary crate.
pub fn nexus_default_root() -> &'static str {
    NEXUS_BASE_URL_DEFAULT
}

pub struct NexusCommand;

impl SlashCommand for NexusCommand {
    fn name(&self) -> &str {
        "nexus"
    }

    fn description(&self) -> &str {
        "Set or clear your Nexus relay key (short for /providers nexus)"
    }

    fn usage(&self) -> &str {
        "/nexus <api_key> [base_url]  |  /nexus clear"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn args_required(&self) -> bool {
        // Bare `/nexus` executes and prints setup guidance (see run()).
        false
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("<api_key> [base_url]   (or: clear)")
    }

    fn suggest_args(&self, _ctx: &AppCtx, args_query: &str) -> Option<Vec<ArgItem>> {
        // Only hint the `clear` verb while the field is still empty; once a key
        // is being typed, get out of the way (keys must not be autocompleted).
        if args_query.trim().is_empty() {
            return Some(vec![ArgItem::new(
                "clear  (remove the stored Nexus key)",
                "clear off logout remove",
                "clear",
                "Removes platform/nexus from ~/.grok/auth.json",
            )]);
        }
        None
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        // Bare `/nexus` → onboarding guidance (how to get a key + how to save it).
        if args.trim().is_empty() {
            return CommandResult::Message(nexus_setup_guide());
        }
        // Delegate to `/providers nexus <args>` so the whole nexus flow — key
        // + optional base_url, `clear`, and persistence to auth.json — lives in
        // exactly one implementation.
        let delegated = format!("nexus {}", args.trim());
        ProvidersCommand.run(ctx, delegated.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::actions::Action;

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
            usage_command_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot::default(),
        }
    }

    #[test]
    fn guide_mentions_gateway_and_commands() {
        let guide = nexus_setup_guide();
        assert!(guide.contains("nexuscore.now"), "guide: {guide}");
        assert!(guide.contains("hyper nexus"), "guide: {guide}");
        assert!(guide.contains("/nexus"), "guide: {guide}");
    }

    #[test]
    fn bare_nexus_shows_guide() {
        let models = ModelState::default();
        let mut ctx = dummy_exec_ctx(&models);
        for empty in ["", "   "] {
            match NexusCommand.run(&mut ctx, empty) {
                CommandResult::Message(msg) => {
                    assert!(msg.contains("nexuscore.now"), "msg: {msg}")
                }
                other => panic!("expected guide Message for {empty:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn nexus_key_delegates_to_provider_save() {
        let models = ModelState::default();
        let mut ctx = dummy_exec_ctx(&models);
        match NexusCommand.run(&mut ctx, "sk-nexus https://gw.example") {
            CommandResult::Action(Action::SetPlatformApiKey {
                platform,
                api_key,
                base_url,
            }) => {
                assert_eq!(platform, "nexus");
                assert_eq!(api_key, "sk-nexus");
                assert_eq!(base_url.as_deref(), Some("https://gw.example"));
            }
            other => panic!("expected SetPlatformApiKey, got {other:?}"),
        }
    }

    #[test]
    fn nexus_clear_delegates_to_provider_clear() {
        let models = ModelState::default();
        let mut ctx = dummy_exec_ctx(&models);
        match NexusCommand.run(&mut ctx, "clear") {
            CommandResult::Action(Action::SetPlatformApiKey {
                platform, api_key, ..
            }) => {
                assert_eq!(platform, "nexus");
                assert!(api_key.is_empty());
            }
            other => panic!("expected clear SetPlatformApiKey, got {other:?}"),
        }
    }
}
