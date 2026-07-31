//! `/scoped-models` — manage the Pi-style model cycle shortlist.
//!
//! Soft list only: does not hard-block `/model`. Patterns are written to
//! `[models].enabled_models` in `~/.grok/config.toml`.

use crate::acp::model_state::{ModelState, platform_lock};
use crate::slash::command::{AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand};

/// Manage `[models].enabled_models` (scoped cycle shortlist).
pub struct ScopedModelsCommand;

impl SlashCommand for ScopedModelsCommand {
    fn name(&self) -> &str {
        "scoped-models"
    }

    fn aliases(&self) -> &[&str] {
        &["scoped", "scope-models", "enabled-models"]
    }

    fn description(&self) -> &str {
        "Manage the scoped model shortlist for Alt+]/Alt+[ cycling"
    }

    fn usage(&self) -> &str {
        "/scoped-models [list|add <id|glob>|remove <id|glob>|clear|set <ids...>]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn args_required(&self) -> bool {
        // Empty → status list (like /providers).
        false
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("[list|add|remove|clear|set] …")
    }

    fn suggest_args(&self, ctx: &AppCtx, args_query: &str) -> Option<Vec<ArgItem>> {
        let (first, rest) = split_first(args_query);
        if first.is_empty() {
            return Some(verb_items());
        }
        if is_verb(first, &["list", "clear", "status", "show"]) {
            return None;
        }
        if is_verb(first, &["add", "remove", "rm", "set"]) {
            if !rest.is_empty() && !rest.ends_with(char::is_whitespace) {
                // Mid-token — no suggestions.
                return None;
            }
            return Some(model_items(ctx.models, first));
        }
        // Partial verb.
        let items: Vec<ArgItem> = verb_items()
            .into_iter()
            .filter(|i| {
                i.insert_text
                    .to_lowercase()
                    .starts_with(&first.to_lowercase())
            })
            .collect();
        if items.is_empty() { None } else { Some(items) }
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        if trimmed.is_empty() || is_verb(trimmed, &["list", "status", "show"]) {
            return CommandResult::Message(render_status(ctx.models));
        }

        let (verb, rest) = split_first(trimmed);
        if is_verb(verb, &["clear"]) {
            if let Err(e) = crate::config_toml_edit::set_enabled_model_ids(&[]) {
                return CommandResult::Error(format!("Failed to clear enabled_models: {e}"));
            }
            return CommandResult::Message(
                "Cleared [models].enabled_models — Alt+]/Alt+[ will cycle all usable models."
                    .into(),
            );
        }

        if is_verb(verb, &["add"]) {
            let pattern = rest.trim();
            if pattern.is_empty() {
                return CommandResult::Error(
                    "Usage: /scoped-models add <model-id-or-glob>\n\
                     Example: /scoped-models add grok-*"
                        .into(),
                );
            }
            if let Err(e) = crate::scoped_models::validate_glob_pattern(pattern) {
                return CommandResult::Error(e);
            }
            let mut list = crate::config_toml_edit::enabled_model_ids();
            if list.iter().any(|p| p == pattern) {
                return CommandResult::Message(format!("Already in shortlist: {pattern}"));
            }
            list.push(pattern.to_string());
            if let Err(e) = crate::config_toml_edit::set_enabled_model_ids(&list) {
                return CommandResult::Error(format!("Failed to write enabled_models: {e}"));
            }
            return CommandResult::Message(format!(
                "Added `{pattern}` to enabled_models ({} pattern(s)). Cycle with Alt+]/Alt+[.",
                list.len()
            ));
        }

        if is_verb(verb, &["remove", "rm"]) {
            let pattern = rest.trim();
            if pattern.is_empty() {
                return CommandResult::Error(
                    "Usage: /scoped-models remove <model-id-or-glob>".into(),
                );
            }
            let mut list = crate::config_toml_edit::enabled_model_ids();
            let before = list.len();
            list.retain(|p| p != pattern);
            if list.len() == before {
                return CommandResult::Error(format!(
                    "Pattern `{pattern}` not in enabled_models. Use /scoped-models list."
                ));
            }
            if let Err(e) = crate::config_toml_edit::set_enabled_model_ids(&list) {
                return CommandResult::Error(format!("Failed to write enabled_models: {e}"));
            }
            return CommandResult::Message(format!("Removed `{pattern}` from enabled_models."));
        }

        if is_verb(verb, &["set"]) {
            let patterns: Vec<String> = rest
                .split_whitespace()
                .map(str::to_owned)
                .filter(|s| !s.is_empty())
                .collect();
            if patterns.is_empty() {
                return CommandResult::Error(
                    "Usage: /scoped-models set <id-or-glob> [more…]\n\
                     Example: /scoped-models set grok-* openai/gpt-5 openrouter/anthropic/*"
                        .into(),
                );
            }
            let bad = crate::scoped_models::invalid_glob_patterns(&patterns);
            if !bad.is_empty() {
                return CommandResult::Error(format!(
                    "Invalid glob pattern(s): {}. Use * and ? (and [...] classes).",
                    bad.join(", ")
                ));
            }
            if let Err(e) = crate::config_toml_edit::set_enabled_model_ids(&patterns) {
                return CommandResult::Error(format!("Failed to write enabled_models: {e}"));
            }
            return CommandResult::Message(format!(
                "Set enabled_models to {} pattern(s):\n  {}",
                patterns.len(),
                patterns.join("\n  ")
            ));
        }

        CommandResult::Error(format!("Unknown subcommand '{verb}'.\n{}", Self.usage()))
    }
}

fn verb_items() -> Vec<ArgItem> {
    [
        ("list", "Show shortlist + matching usable models"),
        ("add", "Append a model id or glob"),
        ("remove", "Remove a pattern from the shortlist"),
        ("set", "Replace the shortlist"),
        ("clear", "Clear shortlist (cycle all usable)"),
    ]
    .into_iter()
    .map(|(name, desc)| ArgItem::new(name, name, name, desc))
    .collect()
}

fn model_items(models: &ModelState, verb: &str) -> Vec<ArgItem> {
    models
        .available
        .iter()
        .filter(|(_, info)| platform_lock(info).is_none())
        .map(|(id, info)| {
            let insert = if verb.eq_ignore_ascii_case("set") {
                format!("set {}", id.0)
            } else if verb.eq_ignore_ascii_case("remove") || verb.eq_ignore_ascii_case("rm") {
                format!("remove {}", id.0)
            } else {
                format!("add {}", id.0)
            };
            let mut item = ArgItem::new(
                info.name.clone(),
                id.0.to_string(),
                insert,
                id.0.to_string(),
            );
            item.action_id = Some(id.0.to_string());
            item
        })
        .collect()
}

fn render_status(models: &ModelState) -> String {
    let patterns = crate::config_toml_edit::enabled_model_ids();
    let candidates = crate::scoped_models::cycle_candidates(models, &patterns);
    let mut out = String::new();
    out.push_str("Scoped models (cycle with Alt+] / Alt+[)\n");
    out.push_str("— soft shortlist; full picker remains /model and Ctrl+M\n\n");
    if patterns.is_empty() {
        out.push_str("enabled_models: (empty — cycling all usable models)\n");
    } else {
        out.push_str(&format!(
            "enabled_models ({}):\n  {}\n",
            patterns.len(),
            patterns.join("\n  ")
        ));
    }
    out.push_str(&format!(
        "\nMatching usable models ({}):\n",
        candidates.len()
    ));
    if candidates.is_empty() {
        out.push_str("  (none — add patterns or configure provider keys)\n");
    } else {
        for id in &candidates {
            let name = models
                .available
                .get(id)
                .map(|i| i.name.as_str())
                .unwrap_or("");
            let cur = if models.current.as_ref() == Some(id) {
                " *"
            } else {
                ""
            };
            out.push_str(&format!("  {}  {}{}\n", id.0, name, cur));
        }
    }
    out.push_str(
        "\nCommands:\n  /scoped-models add <id|glob>\n  /scoped-models remove <id|glob>\n  \
         /scoped-models set <ids…>\n  /scoped-models clear\n",
    );
    out
}

fn split_first(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim_start()),
        None => (s, ""),
    }
}

fn is_verb(tok: &str, verbs: &[&str]) -> bool {
    verbs.iter().any(|v| tok.eq_ignore_ascii_case(v))
}
