//! `/providers` — third-party platform (BYOK) status overview.
//!
//! Lists every registry platform: whether its credential is configured,
//! how many catalog models it offers, and how to unlock it (env var,
//! `[platforms.<id>]` config table, or OAuth). Locked models render dimmed
//! with a 🔒 in `/model`; picking one prints its setup hint.

use xai_grok_models::PlatformId;

use crate::acp::model_state::{ModelState, platform_lock};
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

pub struct ProvidersCommand;

impl SlashCommand for ProvidersCommand {
    fn name(&self) -> &str {
        "providers"
    }

    fn description(&self) -> &str {
        "Show third-party platform (BYOK) status and how to enable them"
    }

    fn usage(&self) -> &str {
        "/providers"
    }

    fn run(&self, ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Message(render_providers(ctx.models))
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
    let envs = platform.api_key_env_names();
    // Prefer the well-known alias (non-GROK name) for brevity.
    let alias = envs
        .iter()
        .find(|e| !e.starts_with("GROK_"))
        .or_else(|| envs.first());
    match alias {
        Some(e) => format!("export {e}=… or [platforms.{}] api_key", platform.as_str()),
        None => format!("[platforms.{}] api_key", platform.as_str()),
    }
}

fn render_providers(models: &ModelState) -> String {
    let mut out = String::new();
    out.push_str("Third-party platforms (BYOK). Locked models show dimmed with 🔒 in /model.\n\n");

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
        "Unlock a platform by exporting its env var or adding `api_key = \"…\"` under \
         `[platforms.<id>]` in ~/.grok/config.toml (env wins). Its models turn selectable in \
         /model immediately after a config reload; see the user guide (25-moonshot, 26-kimi, \
         27-openai-anthropic) for details.",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol as acp;
    use std::sync::Arc;

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
        assert!(out.contains("[platforms.<id>]"));
    }

    #[test]
    fn locked_row_carries_unlock_hint() {
        let mut models = ModelState::default();
        insert_model(&mut models, "deepseek/deepseek-v4-flash", true);
        let out = render_providers(&models);
        assert!(out.contains("DEEPSEEK_API_KEY"), "hint missing: {out}");
        assert!(out.contains("[platforms.deepseek]"), "hint missing: {out}");
    }
}
