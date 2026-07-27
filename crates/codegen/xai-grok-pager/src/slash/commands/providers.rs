//! `/providers` — third-party platform API-key status + setup / logout.
//! Covers both BYOK providers and subscription products such as OpenCode Go.
//!
//! - Bare `/providers` opens an ArgPicker of platforms.
//! - `/providers <platform> <api_key>` saves the key to `~/.grok/auth.json`.
//! - `/providers clear|logout|remove [platform]` removes a stored key
//!   (platform API-key "logout").
//! - OAuth platforms (kimi-code) redirect to `/login kimi` / `grok logout --kimi`.

use xai_grok_models::PlatformId;

use crate::acp::model_state::{ModelState, platform_lock};
use crate::app::actions::Action;
use crate::slash::command::{AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand};

pub struct ProvidersCommand;

impl SlashCommand for ProvidersCommand {
    fn name(&self) -> &str {
        "providers"
    }

    fn aliases(&self) -> &[&str] {
        &["provider"]
    }

    fn description(&self) -> &str {
        "Configure or clear third-party platform API keys"
    }

    fn usage(&self) -> &str {
        "/providers [clear|logout] [platform] [api_key]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn args_required(&self) -> bool {
        // Empty → open platform picker (or subcommand list via suggest).
        true
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("[clear|logout] <platform> [api_key]")
    }

    fn suggest_args(&self, ctx: &AppCtx, args_query: &str) -> Option<Vec<ArgItem>> {
        let (first, rest) = split_first_token(args_query);

        // `/providers clear ` → pick a platform to log out of.
        if is_clear_verb(first) {
            if rest.is_empty() {
                return Some(build_clear_platform_items(ctx.models));
            }
            // Platform already typed after clear — no further suggestions.
            return None;
        }

        // `/providers zai ` → free-type the API key (use `/providers clear zai` to remove).
        if !first.is_empty() && PlatformId::parse(first).is_some() {
            return None;
        }

        // Bare / partial first token: subcommands + platforms.
        let mut items = build_clear_verb_items();
        items.extend(build_platform_items(ctx.models));
        Some(items)
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        if trimmed.is_empty() {
            return CommandResult::Message(render_providers(ctx.models));
        }

        let (first, rest) = split_first_token(trimmed);

        // `/providers clear [platform]` / `logout` / `remove`
        if is_clear_verb(first) {
            let platform_tok = rest.trim();
            if platform_tok.is_empty() {
                return CommandResult::Error(
                    "Usage: /providers clear <platform>\n\
                     Example: /providers clear zai-coding\n\
                     Tip: /providers clear  then pick a platform from the list."
                        .into(),
                );
            }
            // Allow accidental extra tokens: "clear zai-coding now"
            let (plat, _) = split_first_token(platform_tok);
            return clear_platform(plat);
        }

        let Some(platform) = PlatformId::parse(first) else {
            return CommandResult::Error(format!(
                "Unknown platform or command '{first}'.\n\
                 Set key:   /providers <platform> <api_key>\n\
                 Clear key: /providers clear <platform>   (or /providers logout <platform>)"
            ));
        };

        if platform.uses_oauth() {
            let (login, logout) = oauth_login_logout_hint(platform);
            return CommandResult::Error(format!(
                "{} uses OAuth — run {login} to sign in, or `{logout}` to sign out.",
                platform.display_name()
            ));
        }

        let rest = rest.trim();
        if rest.is_empty() {
            return CommandResult::Error(format!(
                "Paste an API key after the platform name:\n  /providers {} <api_key>\n\
                 Or clear a stored key with:\n  /providers clear {}\n  /providers {} clear",
                platform.as_str(),
                platform.as_str(),
                platform.as_str(),
            ));
        }

        // Nexus is self-hosted: accept an optional gateway base_url after the
        // key (`/providers nexus <api_key> [base_url]`). Other platforms use the
        // whole remainder as the key.
        let (api_key, base_url) = if platform == PlatformId::Nexus {
            let (key, base) = split_first_token(rest);
            let base = base.trim();
            (key, (!base.is_empty()).then(|| base.to_owned()))
        } else {
            (rest, None)
        };

        if is_clear_verb(api_key) || is_clear_verb(split_first_token(api_key).0) {
            return clear_platform(platform.as_str());
        }

        CommandResult::Action(Action::SetPlatformApiKey {
            platform: platform.as_str().to_owned(),
            api_key: api_key.to_owned(),
            base_url,
        })
    }
}

fn clear_platform(platform_tok: &str) -> CommandResult {
    let Some(platform) = PlatformId::parse(platform_tok) else {
        return CommandResult::Error(format!(
            "Unknown platform '{platform_tok}'. Run /providers clear and pick one."
        ));
    };
    if platform.uses_oauth() {
        let (_, logout) = oauth_login_logout_hint(platform);
        return CommandResult::Error(format!(
            "{} uses OAuth — run `{logout}` (not /providers clear).",
            platform.display_name()
        ));
    }
    CommandResult::Action(Action::SetPlatformApiKey {
        platform: platform.as_str().to_owned(),
        api_key: String::new(),
        base_url: None,
    })
}

/// Per-platform OAuth login/logout commands shown in error hints.
///
/// Defined in [`super::oauth_login_logout_hint`] and shared with `/logout`.
fn oauth_login_logout_hint(platform: PlatformId) -> (&'static str, &'static str) {
    super::oauth_login_logout_hint(platform)
}

fn is_clear_verb(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "clear" | "logout" | "remove" | "unset" | "delete" | "off"
    )
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

fn platform_status(models: &ModelState, platform: PlatformId) -> (PlatformStatus, usize, usize) {
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
        let (login, _) = oauth_login_logout_hint(platform);
        return format!("{login} (OAuth)");
    }
    // Nexus is self-hosted → the gateway root is an optional trailing arg.
    if platform == PlatformId::Nexus {
        return format!("/providers {} <api_key> [base_url]", platform.as_str());
    }
    format!("/providers {} <api_key>", platform.as_str())
}

fn build_clear_verb_items() -> Vec<ArgItem> {
    vec![ArgItem::new(
        "clear / logout  (remove stored API key)",
        "clear logout remove unset delete",
        "clear ",
        "Pick a platform next — removes platform/<id> from ~/.grok/auth.json",
    )]
}

fn build_clear_platform_items(models: &ModelState) -> Vec<ArgItem> {
    PlatformId::ALL
        .into_iter()
        .filter(|p| !p.uses_oauth())
        .map(|platform| {
            let (status, usable, locked) = platform_status(models, platform);
            let total = usable + locked;
            let desc = match status {
                PlatformStatus::Ready => {
                    format!("✓ {total} models currently unlocked — clear stored key")
                }
                PlatformStatus::Locked => {
                    format!("🔒 {total} models locked — clear stored key anyway")
                }
                PlatformStatus::NoCatalog => "clear stored key if present".to_string(),
            };
            ArgItem::new(
                format!("{}  {}", platform.as_str(), platform.display_name()),
                platform.as_str(),
                // No trailing space — Enter runs `/providers clear <id>` immediately.
                platform.as_str(),
                desc,
            )
        })
        .collect()
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
                    format!(
                        "{total} models ready — re-paste key to replace, or /providers clear {}",
                        platform.as_str()
                    ),
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
        "Third-party platforms (BYOK and subscription API keys).\n\
         Set key:    /providers <platform> <api_key>\n\
         Clear key:  /providers clear <platform>   (alias: logout / remove)\n\n",
    );

    let mut any_ready = false;
    let mut any_locked = false;
    for platform in PlatformId::ALL {
        let (status, usable, locked) = platform_status(models, platform);
        let total = usable + locked;
        let (icon, models_col, tail) = match status {
            PlatformStatus::Ready => {
                any_ready = true;
                (
                    "✓",
                    format!("{total} models"),
                    format!(" — /providers clear {}", platform.as_str()),
                )
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
        "Keys live in ~/.grok/auth.json under platform/<id>. Env vars still win over the \
         stored key (e.g. ZAI_API_KEY overrides /providers zai-coding). After clear, also \
         unset conflicting env vars if models stay unlocked.",
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
            billing_surface_visible: true,
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
        assert!(out.contains("/providers clear"));
    }

    #[test]
    fn locked_row_carries_unlock_hint() {
        let mut models = ModelState::default();
        insert_model(&mut models, "deepseek/deepseek-v4-flash", true);
        let out = render_providers(&models);
        assert!(out.contains("/providers deepseek"), "hint missing: {out}");
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
            CommandResult::Action(Action::SetPlatformApiKey {
                platform,
                api_key,
                base_url,
            }) => {
                assert_eq!(platform, "zai");
                assert_eq!(api_key, "sk-test-key");
                // Non-Nexus platforms never carry a base_url.
                assert_eq!(base_url, None);
            }
            other => panic!("expected SetPlatformApiKey, got {other:?}"),
        }
    }

    #[test]
    fn run_opencode_go_emits_api_key_storage_action() {
        let models = ModelState::default();
        let mut ctx = dummy_exec_ctx(&models);
        match ProvidersCommand.run(&mut ctx, "opencode-go oc-test-key") {
            CommandResult::Action(Action::SetPlatformApiKey {
                platform,
                api_key,
                base_url,
            }) => {
                assert_eq!(platform, "opencode-go");
                assert_eq!(api_key, "oc-test-key");
                assert_eq!(base_url, None);
            }
            other => panic!("expected SetPlatformApiKey, got {other:?}"),
        }
    }

    #[test]
    fn run_nexus_parses_optional_base_url() {
        let models = ModelState::default();
        let mut ctx = dummy_exec_ctx(&models);
        // With a base_url token.
        match ProvidersCommand.run(&mut ctx, "nexus sk-nexus https://nexuscore.now") {
            CommandResult::Action(Action::SetPlatformApiKey {
                platform,
                api_key,
                base_url,
            }) => {
                assert_eq!(platform, "nexus");
                assert_eq!(api_key, "sk-nexus");
                assert_eq!(base_url.as_deref(), Some("https://nexuscore.now"));
            }
            other => panic!("expected SetPlatformApiKey, got {other:?}"),
        }
        // Without a base_url token → None (env/compiled default is used).
        match ProvidersCommand.run(&mut ctx, "nexus sk-nexus") {
            CommandResult::Action(Action::SetPlatformApiKey {
                platform,
                api_key,
                base_url,
            }) => {
                assert_eq!(platform, "nexus");
                assert_eq!(api_key, "sk-nexus");
                assert_eq!(base_url, None);
            }
            other => panic!("expected SetPlatformApiKey, got {other:?}"),
        }
    }

    #[test]
    fn run_clear_suffix_sends_empty_key() {
        let models = ModelState::default();
        let mut ctx = dummy_exec_ctx(&models);
        match ProvidersCommand.run(&mut ctx, "zai clear") {
            CommandResult::Action(Action::SetPlatformApiKey { platform, api_key, .. }) => {
                assert_eq!(platform, "zai");
                assert!(api_key.is_empty());
            }
            other => panic!("expected clear SetPlatformApiKey, got {other:?}"),
        }
    }

    #[test]
    fn run_clear_subcommand_sends_empty_key() {
        let models = ModelState::default();
        let mut ctx = dummy_exec_ctx(&models);
        for cmd in ["clear zai-coding", "logout zai-coding", "remove zai-coding"] {
            match ProvidersCommand.run(&mut ctx, cmd) {
                CommandResult::Action(Action::SetPlatformApiKey { platform, api_key, .. }) => {
                    assert_eq!(platform, "zai-coding", "cmd={cmd}");
                    assert!(api_key.is_empty(), "cmd={cmd}");
                }
                other => panic!("expected clear for {cmd}, got {other:?}"),
            }
        }
    }

    #[test]
    fn run_clear_without_platform_errors() {
        let models = ModelState::default();
        let mut ctx = dummy_exec_ctx(&models);
        match ProvidersCommand.run(&mut ctx, "clear") {
            CommandResult::Error(msg) => assert!(msg.contains("clear <platform>"), "{msg}"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn suggest_args_includes_clear_verb() {
        let models = ModelState::default();
        let ctx = AppCtx {
            models: &models,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            workflows_available: true,
            screen_mode: crate::app::ScreenMode::Inline,
        };
        let items = ProvidersCommand.suggest_args(&ctx, "").expect("items");
        assert!(
            items.iter().any(|i| i.insert_text.starts_with("clear")),
            "missing clear verb: {:?}",
            items.iter().map(|i| &i.insert_text).collect::<Vec<_>>()
        );
        assert!(items.iter().any(|i| i.insert_text.starts_with("zai")));
    }

    #[test]
    fn suggest_args_after_clear_lists_platforms() {
        let models = ModelState::default();
        let ctx = AppCtx {
            models: &models,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            workflows_available: true,
            screen_mode: crate::app::ScreenMode::Inline,
        };
        let items = ProvidersCommand
            .suggest_args(&ctx, "clear ")
            .expect("clear platform list");
        assert!(items.iter().any(|i| i.insert_text == "zai-coding"));
        assert!(items.iter().all(|i| !i.insert_text.contains("kimi-code")));
    }
}
