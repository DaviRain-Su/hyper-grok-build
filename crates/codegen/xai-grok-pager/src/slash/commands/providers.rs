//! `/providers` — third-party platform (BYOK) status + API key setup.
//!
//! - Bare `/providers` (or incomplete args) opens an ArgPicker of platforms.
//! - `/providers <platform> <api_key>` saves the key to `~/.grok/auth.json`
//!   and restamps the model catalog so locked models unlock.
//! - OAuth platforms (kimi-code) redirect to `/login kimi`.

use xai_grok_models::PlatformId;

use crate::acp::model_state::{ModelState, platform_lock};
use crate::app::actions::Action;
use crate::slash::command::{
    AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand,
};

pub struct ProvidersCommand;

impl SlashCommand for ProvidersCommand {
    fn name(&self) -> &str {
        "providers"
    }

    fn description(&self) -> &str {
        "Configure third-party platform API keys (or show status)"
    }

    fn usage(&self) -> &str {
        "/providers [platform] [api_key]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn args_required(&self) -> bool {
        // Empty → open platform picker. Picking inserts platform id; user
        // pastes the key and hits Enter again.
        true
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("<platform> <api_key>")
    }

    fn suggest_args(&self, ctx: &AppCtx, args_query: &str) -> Option<Vec<ArgItem>> {
        // Once a platform is already chosen as the first token, free-type the
        // API key (no second suggestion list — paste + Enter).
        let (first, rest) = split_first_token(args_query);
        if !first.is_empty() && PlatformId::parse(first).is_some() && !rest.is_empty() {
            return None;
        }
        if !first.is_empty() && PlatformId::parse(first).is_some() && rest.is_empty() {
            // Platform selected; waiting for key — no suggestion rows.
            return None;
        }
        Some(build_platform_items(ctx.models))
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        if trimmed.is_empty() {
            // ArgPicker should have opened; if run still fires bare, show status.
            return CommandResult::Message(render_providers(ctx.models));
        }

        let (platform_tok, key_rest) = split_first_token(trimmed);
        let Some(platform) = PlatformId::parse(platform_tok) else {
            return CommandResult::Error(format!(
                "Unknown platform '{platform_tok}'. Run /providers to pick one."
            ));
        };

        if platform.uses_oauth() {
            return CommandResult::Error(format!(
                "{} uses OAuth — run /login kimi (not an API key).",
                platform.display_name()
            ));
        }

        let api_key = key_rest.trim();
        if api_key.is_empty() {
            return CommandResult::Error(format!(
                "Paste an API key after the platform name:\n  /providers {} <api_key>\n\
                 Or clear a stored key with: /providers {} clear",
                platform.as_str(),
                platform.as_str()
            ));
        }

        let clear = api_key.eq_ignore_ascii_case("clear")
            || api_key.eq_ignore_ascii_case("none")
            || api_key.eq_ignore_ascii_case("remove");
        CommandResult::Action(Action::SetPlatformApiKey {
            platform: platform.as_str().to_owned(),
            api_key: if clear {
                String::new()
            } else {
                api_key.to_owned()
            },
        })
    }
}

fn split_first_token(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.split_once(char::is_whitespace) {
        Some((a, b)) => (a, b.trim_start()),
        None => (s, ""),
    }
}

/// Per-platform status derived from the live catalog projection.
enum PlatformStatus {
    /// At least one catalog model is usable (credential resolved).
    Ready,
    /// Catalog models exist but all are locked (no credential).
    Locked,
    /// No catalog entries (reserved platform, or catalog not loaded yet).
    NoCatalog,
}

fn platform_status(
    models: &ModelState,
    platform: PlatformId,
) -> (PlatformStatus, usize, usize) {
    let prefix = format!("{}/", platform.as_str());
    let mut usable = 0usize;
    let mut locked = 0usize;
    for (id, info) in &models.available {
        if !id.0.as_ref().starts_with(&prefix) {
            continue;
        }
        if platform_lock(info).is_some() {
            locked += 1;
        } else {
            usable += 1;
        }
    }
    let status = if usable > 0 {
        PlatformStatus::Ready
    } else if locked > 0 {
        PlatformStatus::Locked
    } else {
        PlatformStatus::NoCatalog
    };
    (status, usable, locked)
}

/// Compact one-line unlock instruction for the table.
fn compact_hint(platform: PlatformId) -> String {
    if platform.uses_oauth() {
        return "/login kimi (OAuth)".to_string();
    }
    format!("/providers {} <api_key>", platform.as_str())
}

fn build_platform_items(models: &ModelState) -> Vec<ArgItem> {
    PlatformId::ALL
        .into_iter()
        .map(|platform| {
            let (status, usable, locked) = platform_status(models, platform);
            let total = usable + locked;
            let (icon, desc) = match status {
                PlatformStatus::Ready => (
                    "✓",
                    format!("{total} models ready — re-paste key to replace"),
                ),
                PlatformStatus::Locked if platform.uses_oauth() => {
                    ("🔒", format!("{total} models — run /login kimi"))
                }
                PlatformStatus::Locked => (
                    "🔒",
                    format!("{total} models — paste API key after selecting"),
                ),
                PlatformStatus::NoCatalog if platform.uses_oauth() => {
                    ("—", "OAuth — run /login kimi".to_string())
                }
                PlatformStatus::NoCatalog => (
                    "—",
                    "no catalog models yet — paste API key to enable".to_string(),
                ),
            };
            ArgItem::new(
                format!("{icon} {}  {}", platform.as_str(), platform.display_name()),
                platform.as_str(),
                // Trailing space so after pick the prompt is ready for the key.
                format!("{} ", platform.as_str()),
                desc,
            )
        })
        .collect()
}

fn render_providers(models: &ModelState) -> String {
    let mut out = String::new();
    out.push_str(
        "Third-party platforms (BYOK). Select one, then paste an API key:\n\
         /providers <platform> <api_key>\n\n",
    );

    let mut any_ready = false;
    let mut any_locked = false;
    for platform in PlatformId::ALL {
        let (status, usable, locked) = platform_status(models, platform);
        let total = usable + locked;
        let (icon, models_col, tail) = match status {
            PlatformStatus::Ready => {
                any_ready = true;
                ("✓", format!("{total} models"), String::new())
            }
            PlatformStatus::Locked => {
                any_locked = true;
                (
                    "🔒",
                    format!("{total} models"),
                    format!(" — {}", compact_hint(platform)),
                )
            }
            PlatformStatus::NoCatalog => ("—", "no catalog models".to_string(), String::new()),
        };
        out.push_str(&format!(
            " {icon} {:<14} {:<26} {models_col}{tail}\n",
            platform.as_str(),
            platform.display_name(),
        ));
    }

    out.push('\n');
    if !any_ready && !any_locked {
        out.push_str(
            "Model catalog not loaded in this view yet — statuses appear once a session connects.\n",
        );
    }
    out.push_str(
        "Keys are stored in ~/.grok/auth.json (scope platform/<id>). Env vars still win \
         over the stored key; config.toml [platforms.<id>] api_key is the final fallback. \
         Clear with: /providers <platform> clear",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol as acp;
    use std::sync::Arc;

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
            pager_state: crate::settings::PagerLocalSnapshot::default(),
        }
    }

    fn insert_model(models: &mut ModelState, id: &str, locked: bool) {
        let mid = acp::ModelId::new(Arc::from(id));
        let meta = locked.then(|| {
            serde_json::json!({
                "requiresApiKey": true,
                "platform": "deepseek",
                "platformName": "DeepSeek",
                "apiKeyEnv": ["GROK_DEEPSEEK_API_KEY", "DEEPSEEK_API_KEY"],
                "setupHint": "export …",
            })
            .as_object()
            .cloned()
            .unwrap()
        });
        models.available.insert(
            mid.clone(),
            acp::ModelInfo::new(mid, id.to_string()).meta(meta),
        );
    }

    #[test]
    fn status_reflects_lock_state() {
        let mut models = ModelState::default();
        insert_model(&mut models, "deepseek/deepseek-v4-flash", true);
        insert_model(&mut models, "openai/gpt-5", false);

        let (status, usable, locked) = platform_status(&models, PlatformId::DeepSeek);
        assert!(matches!(status, PlatformStatus::Locked));
        assert_eq!((usable, locked), (0, 1));

        let (status, usable, locked) = platform_status(&models, PlatformId::OpenAi);
        assert!(matches!(status, PlatformStatus::Ready));
        assert_eq!((usable, locked), (1, 0));

        let (status, _, _) = platform_status(&models, PlatformId::Mistral);
        assert!(matches!(status, PlatformStatus::NoCatalog));
    }

    #[test]
    fn render_lists_all_registry_platforms() {
        let models = ModelState::default();
        let out = render_providers(&models);
        for platform in PlatformId::ALL {
            assert!(
                out.contains(platform.as_str()),
                "missing platform row: {}",
                platform.as_str()
            );
        }
        assert!(out.contains("/providers"));
    }

    #[test]
    fn locked_row_carries_unlock_hint() {
        let mut models = ModelState::default();
        insert_model(&mut models, "deepseek/deepseek-v4-flash", true);
        let out = render_providers(&models);
        assert!(
            out.contains("/providers deepseek"),
            "hint missing: {out}"
        );
    }

    #[test]
    fn run_rejects_oauth_platform() {
        let models = ModelState::default();
        let mut ctx = dummy_exec_ctx(&models);
        match ProvidersCommand.run(&mut ctx, "kimi-code sk-fake") {
            CommandResult::Error(msg) => assert!(msg.contains("/login kimi"), "{msg}"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn run_emits_set_platform_api_key() {
        let models = ModelState::default();
        let mut ctx = dummy_exec_ctx(&models);
        match ProvidersCommand.run(&mut ctx, "zai sk-test-key") {
            CommandResult::Action(Action::SetPlatformApiKey { platform, api_key }) => {
                assert_eq!(platform, "zai");
                assert_eq!(api_key, "sk-test-key");
            }
            other => panic!("expected SetPlatformApiKey, got {other:?}"),
        }
    }

    #[test]
    fn run_clear_sends_empty_key() {
        let models = ModelState::default();
        let mut ctx = dummy_exec_ctx(&models);
        match ProvidersCommand.run(&mut ctx, "zai clear") {
            CommandResult::Action(Action::SetPlatformApiKey { platform, api_key }) => {
                assert_eq!(platform, "zai");
                assert!(api_key.is_empty());
            }
            other => panic!("expected clear SetPlatformApiKey, got {other:?}"),
        }
    }

    #[test]
    fn suggest_args_lists_platforms() {
        let models = ModelState::default();
        let ctx = AppCtx {
            models: &models,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            screen_mode: crate::app::ScreenMode::Inline,
        };
        let items = ProvidersCommand.suggest_args(&ctx, "").expect("items");
        assert_eq!(items.len(), PlatformId::ALL.len());
        assert!(items.iter().any(|i| i.insert_text.starts_with("zai")));
    }
}
