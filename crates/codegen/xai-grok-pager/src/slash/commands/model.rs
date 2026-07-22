//! `/model` (alias `/m`) — switch model + (optionally) reasoning effort.
//! Chained autocomplete: pick a reasoning-supported model → trailing space
//! re-opens the dropdown into a `low|medium|high|xhigh` sub-menu.
//!
//! Multi-provider display: managed platform models (`{platform}/{model}`) show
//! the provider on the right. When several rows share a display name (e.g.
//! GLM-5.2 on Z.AI Coding Plan CN and on Ollama), the left label is
//! disambiguated and `insert_text` uses the catalog id so selection is unique.

use agent_client_protocol as acp;
use xai_grok_models::parse_managed_model_key;
use xai_grok_shell::sampling::types::{PlatformLockMeta, supports_reasoning_effort_meta};

use crate::acp::model_state::{ModelState, platform_lock};
use crate::app::actions::Action;
use crate::slash::command::{AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand};
use crate::slash::commands::effort_levels::build_effort_arg_items;

/// Switch the active model (and optionally its reasoning effort).
pub struct ModelCommand;

impl SlashCommand for ModelCommand {
    fn name(&self) -> &str {
        "model"
    }

    fn aliases(&self) -> &[&str] {
        &["m"]
    }

    fn description(&self) -> &str {
        "Switch the active model"
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn offered_when_session_less(&self) -> bool {
        // The dashboard offers `/model` to pick the model for the next
        // spawned agent (intercepted in `dispatch_dashboard_dispatch_slash`).
        true
    }

    fn usage(&self) -> &str {
        "/model <name> [effort]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn args_required(&self) -> bool {
        true
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("<model> [effort]")
    }

    fn suggest_args(&self, ctx: &AppCtx, args_query: &str) -> Option<Vec<ArgItem>> {
        if ctx.models.is_empty() {
            return None;
        }

        // Effort phase if input is "<reasoning-model> ", else model phase.
        if let Some(model_id) = detect_effort_phase(ctx.models, args_query) {
            return Some(build_effort_items(ctx.models, &model_id));
        }
        Some(build_model_items(ctx.models))
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        if trimmed.is_empty() {
            return CommandResult::Error("Usage: /model <name> [effort]".into());
        }

        // Prefer an exact full-string catalog match first. Model display names
        // often contain spaces ("Grok 4.5"); if we split on the last token
        // first, a shorter catalog entry ("Grok") would steal the prefix and
        // treat "4.5" as an effort level.
        if let Some(id) = ctx.models.resolve_by_name_or_id(trimmed) {
            if let Some(lock) = locked_model(ctx.models, &id) {
                return CommandResult::Error(lock_message(&lock, trimmed));
            }
            return CommandResult::Action(Action::SetDefaultModel(id));
        }
        if let Some(msg) = ambiguous_model_message(ctx.models, trimmed) {
            return CommandResult::Error(msg);
        }

        // Trailing effort token + reasoning model → session-scoped switch
        // (not persisted as default). Resolve via the shared gate so a rejected
        // level (e.g. `none` on grok-4.5) surfaces the effort error with the
        // model's offered ids — not "Unknown model: … none".
        if let Some((prefix, token)) = split_trailing_token(trimmed)
            && let Some(id) = resolve_model(ctx.models, prefix)
            && ctx
                .models
                .available
                .get(&id)
                .map(supports_reasoning_effort)
                .unwrap_or(false)
        {
            if let Some(lock) = locked_model(ctx.models, &id) {
                return CommandResult::Error(lock_message(&lock, prefix));
            }
            return match ctx.models.resolve_effort_for_model(&id, token) {
                Ok(effort) => CommandResult::Action(Action::SwitchModel {
                    model_id: id,
                    effort: Some(effort),
                }),
                Err(err) => CommandResult::Error(err.message()),
            };
        }
        if let Some((prefix, _)) = split_trailing_token(trimmed)
            && let Some(msg) = ambiguous_model_message(ctx.models, prefix)
        {
            return CommandResult::Error(msg);
        }

        CommandResult::Error(format!("Unknown model: {trimmed}"))
    }
}

/// Lock metadata when `id` is a credential-less managed platform model.
fn locked_model(models: &ModelState, id: &acp::ModelId) -> Option<PlatformLockMeta> {
    models.available.get(id).and_then(platform_lock)
}

/// User-facing rejection for picking a locked model: what to configure.
fn lock_message(lock: &PlatformLockMeta, requested: &str) -> String {
    let provider = if lock.platform_name.is_empty() {
        lock.platform.as_str()
    } else {
        lock.platform_name.as_str()
    };
    format!(
        "'{requested}' is provided by {provider}, which is not configured yet. \
         To enable it: {}. See /providers for all platforms.",
        lock.setup_hint
    )
}

/// Look up a model by case-insensitive display name OR model id match.
fn resolve_model(models: &ModelState, name: &str) -> Option<acp::ModelId> {
    models.resolve_by_name_or_id(name)
}

fn supports_reasoning_effort(info: &acp::ModelInfo) -> bool {
    supports_reasoning_effort_meta(info.meta.as_ref())
}

/// Provider label for a catalog row: lock meta, managed platform key, or none.
fn provider_label(id: &acp::ModelId, info: &acp::ModelInfo) -> Option<String> {
    if let Some(lock) = platform_lock(info) {
        let name = if lock.platform_name.is_empty() {
            lock.platform
        } else {
            lock.platform_name
        };
        return Some(name);
    }
    parse_managed_model_key(id.0.as_ref()).map(|(platform, _)| platform.display_name().to_string())
}

/// Whether `description` is the generic Pi catalog source stamp (not useful in
/// the picker UI — the provider column already conveys the same signal).
fn is_generic_pi_catalog_description(description: &str) -> bool {
    description
        .trim()
        .to_ascii_lowercase()
        .starts_with("official pi catalog")
}

/// Token inserted into `/model …` so selection stays unambiguous.
///
/// Managed platform rows (`{platform}/{model}`) always use the catalog id —
/// several providers can ship the same display name (GLM-5.2 on Z.AI CN and
/// Ollama). Non-managed rows use the friendly display name unless that name
/// collides with another catalog entry.
fn model_arg_token(id: &acp::ModelId, info: &acp::ModelInfo, name_collides: bool) -> String {
    if parse_managed_model_key(id.0.as_ref()).is_some() || name_collides {
        id.0.to_string()
    } else {
        info.name.clone()
    }
}

/// Right-column text: prefer a human provider name for managed/locked rows;
/// otherwise keep a non-generic catalog description when present.
fn model_row_description(
    info: &acp::ModelInfo,
    provider: Option<&str>,
    locked_setup_hint: Option<&str>,
) -> String {
    if let Some(hint) = locked_setup_hint {
        return hint.to_string();
    }
    if let Some(provider) = provider {
        return provider.to_string();
    }
    info.description
        .as_deref()
        .filter(|d| !d.is_empty() && !is_generic_pi_catalog_description(d))
        .unwrap_or("")
        .to_string()
}

/// Error when the typed display name matches multiple configured providers.
fn ambiguous_model_message(models: &ModelState, query: &str) -> Option<String> {
    let matches = models.ids_matching_name(query);
    if matches.len() < 2 {
        return None;
    }
    let mut options: Vec<String> = matches
        .iter()
        .map(|id| {
            let provider = models
                .available
                .get(id)
                .and_then(|info| provider_label(id, info))
                .unwrap_or_else(|| id.0.to_string());
            format!("  {provider}  →  /model {}", id.0)
        })
        .collect();
    options.sort();
    Some(format!(
        "'{query}' matches multiple providers:\n{}\nPick one by id.",
        options.join("\n")
    ))
}

/// Split `args` into `(prefix, last_token)` on the final whitespace run.
/// Returns `None` when there is no interior whitespace to split on. The token is
/// resolved to an effort against the picked model's options by the caller.
fn split_trailing_token(args: &str) -> Option<(&str, &str)> {
    let (prefix, last) = args.rsplit_once(char::is_whitespace)?;
    let prefix = prefix.trim_end();
    if prefix.is_empty() || last.is_empty() {
        return None;
    }
    Some((prefix, last))
}

/// True when `args_query` starts with `token` (case-insensitive) followed by
/// whitespace — the shape used to enter the effort sub-menu.
fn starts_with_token_then_ws(args_query: &str, token: &str) -> bool {
    args_query.len() > token.len()
        && args_query.is_char_boundary(token.len())
        && args_query[..token.len()].eq_ignore_ascii_case(token)
        && args_query[token.len()..].starts_with(char::is_whitespace)
}

/// Returns the matched model id when `args_query` is `"<model-token> ..."`.
/// Matches catalog ids first (unique), then unique display names (longest
/// first so `"Grok 4.5"` wins over `"Grok"`). Locked platform models are
/// excluded — picking one must hit the lock error in `run()`, not chain into
/// an effort sub-menu.
fn detect_effort_phase(models: &ModelState, args_query: &str) -> Option<acp::ModelId> {
    let usable: Vec<(&acp::ModelId, &acp::ModelInfo)> = models
        .available
        .iter()
        .filter(|(_, info)| supports_reasoning_effort(info) && platform_lock(info).is_none())
        .collect();

    // Catalog id tokens (`platform/model`) — always unique.
    let mut id_tokens: Vec<(&acp::ModelId, &str)> = usable
        .iter()
        .map(|(id, _)| (*id, id.0.as_ref()))
        .collect();
    id_tokens.sort_by_key(|(_, token)| std::cmp::Reverse(token.len()));
    for (id, token) in id_tokens {
        if starts_with_token_then_ws(args_query, token) {
            return Some(id.clone());
        }
    }

    // Display-name tokens only when the name is unique in the full catalog
    // (including locked rows), so we never open effort for an ambiguous label.
    let mut name_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for info in models.available.values() {
        *name_counts
            .entry(info.name.to_ascii_lowercase())
            .or_default() += 1;
    }
    let mut name_tokens: Vec<(&acp::ModelId, &str)> = usable
        .iter()
        .filter(|(_, info)| {
            name_counts
                .get(&info.name.to_ascii_lowercase())
                .copied()
                .unwrap_or(0)
                == 1
        })
        .map(|(id, info)| (*id, info.name.as_str()))
        .collect();
    name_tokens.sort_by_key(|(_, name)| std::cmp::Reverse(name.len()));
    for (id, name) in name_tokens {
        if starts_with_token_then_ws(args_query, name) {
            return Some(id.clone());
        }
    }
    None
}

/// One row per logical model. Reasoning models get a trailing space in
/// `insert_text` so the prompt widget chains into the effort sub-menu.
///
/// Locked platform models (provider credential not configured) sort after all
/// usable models, render with a 🔒 prefix and carry the setup hint as their
/// description; their `insert_text` never chains to the effort phase.
///
/// Managed / multi-provider rows show the provider on the right. When several
/// rows share a display name, the left label is also disambiguated and
/// `insert_text` uses the catalog id.
fn build_model_items(models: &ModelState) -> Vec<ArgItem> {
    let current_id = models.current.as_ref();
    let mut name_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for info in models.available.values() {
        *name_counts
            .entry(info.name.to_ascii_lowercase())
            .or_default() += 1;
    }

    let mut items: Vec<ArgItem> = Vec::with_capacity(models.available.len());
    let mut locked_items: Vec<ArgItem> = Vec::new();
    for (id, info) in &models.available {
        let is_current = current_id == Some(id);
        let name_collides = name_counts
            .get(&info.name.to_ascii_lowercase())
            .copied()
            .unwrap_or(0)
            > 1;
        let provider = provider_label(id, info);
        let token = model_arg_token(id, info, name_collides);

        if let Some(lock) = platform_lock(info) {
            let provider = provider.as_deref().unwrap_or(lock.platform.as_str());
            locked_items.push(ArgItem {
                display: format!("🔒 {} — {provider}", info.name),
                match_text: format!("{} {provider} {}", info.name, id.0),
                // Catalog id keeps locked multi-provider rows distinct if the
                // user types the token before credentials are configured.
                insert_text: token,
                description: lock.setup_hint.clone(),
                locked: true,
                action_id: Some(id.0.to_string()),
                hidden: false,
            });
            continue;
        }

        let supports = supports_reasoning_effort(info);
        let display = match (name_collides, provider.as_deref(), is_current) {
            (true, Some(p), true) => format!("{} — {p} (current)", info.name),
            (true, Some(p), false) => format!("{} — {p}", info.name),
            (true, None, true) => format!("{} — {} (current)", info.name, id.0),
            (true, None, false) => format!("{} — {}", info.name, id.0),
            (false, _, true) => format!("{} (current)", info.name),
            (false, _, false) => info.name.clone(),
        };

        // Trailing space on reasoning models: signals "more input
        // expected" to the prompt widget so Enter advances to effort
        // phase instead of submitting.
        let insert_text = if supports {
            format!("{token} ")
        } else {
            token
        };

        let match_text = match provider.as_deref() {
            Some(p) => format!("{} {p} {}", info.name, id.0),
            None => info.name.clone(),
        };

        items.push(ArgItem {
            display,
            match_text,
            insert_text,
            description: model_row_description(info, provider.as_deref(), None),
            locked: false,
            action_id: Some(id.0.to_string()),
            hidden: false,
        });
    }
    items.extend(locked_items);
    items
}

/// One row per effort level for the `/model` chained effort phase.
/// `insert_text` is `"<model-token> high"` so selecting a row completes both
/// tokens with the same unique token used in the model phase.
fn build_effort_items(models: &ModelState, model_id: &acp::ModelId) -> Vec<ArgItem> {
    let info = match models.available.get(model_id) {
        Some(info) => info,
        None => return Vec::new(),
    };
    let name_collides = models
        .available
        .values()
        .filter(|i| i.name.eq_ignore_ascii_case(&info.name))
        .count()
        > 1;
    let token = model_arg_token(model_id, info, name_collides);
    let is_current_model = models.current.as_ref() == Some(model_id);
    let options = models.reasoning_effort_options_for(model_id);
    build_effort_arg_items(
        &options,
        models.reasoning_effort,
        is_current_model,
        |option| format!("{token} {}", option.id),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use xai_grok_shell::sampling::types::ReasoningEffort;

    fn model_with_reasoning(id: &str, name: &str) -> (acp::ModelId, acp::ModelInfo) {
        let id = acp::ModelId::new(Arc::from(id));
        let mut meta = serde_json::Map::new();
        meta.insert(
            "supportsReasoningEffort".into(),
            serde_json::Value::Bool(true),
        );
        let info = acp::ModelInfo::new(id.clone(), name.to_string())
            .meta(serde_json::Value::Object(meta).as_object().cloned());
        (id, info)
    }

    fn plain_model(id: &str, name: &str) -> (acp::ModelId, acp::ModelInfo) {
        let id = acp::ModelId::new(Arc::from(id));
        let info = acp::ModelInfo::new(id.clone(), name.to_string());
        (id, info)
    }

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
            pager_state: crate::settings::PagerLocalSnapshot {
                multiline_mode: false,
                yolo_mode: false,
                ..crate::settings::PagerLocalSnapshot::default()
            },
        }
    }

    #[test]
    fn split_trailing_token_splits_on_final_whitespace() {
        assert_eq!(
            split_trailing_token("Reasoning X high"),
            Some(("Reasoning X", "high"))
        );
        assert_eq!(
            split_trailing_token("reasoning-x  xhigh"),
            Some(("reasoning-x", "xhigh"))
        );
        // No interior whitespace → nothing to split off.
        assert!(split_trailing_token("reasoning-x-pro").is_none());
    }

    #[test]
    fn empty_query_returns_one_row_per_logical_model() {
        let mut state = ModelState::default();
        let (rid, rinfo) = model_with_reasoning("reasoning-x", "Reasoning X");
        let (pid, pinfo) = plain_model("grok-4.5", "Grok 4.5");
        state.available.insert(rid, rinfo);
        state.available.insert(pid, pinfo);

        let cmd = ModelCommand;
        let ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            workflows_available: true,
            screen_mode: crate::app::ScreenMode::Fullscreen,
        };
        let items = cmd.suggest_args(&ctx, "").unwrap();
        assert_eq!(items.len(), 2, "model phase: one row per logical model");

        // Reasoning model has trailing space in insert_text -- this is the
        // signal the prompt widget reads to keep the dropdown open after
        // Enter so the effort sub-menu can render.
        let reasoning = items
            .iter()
            .find(|i| i.match_text == "Reasoning X")
            .unwrap();
        assert_eq!(reasoning.insert_text, "Reasoning X ");

        // Plain model has no trailing space -- Enter commits immediately.
        let plain = items.iter().find(|i| i.match_text == "Grok 4.5").unwrap();
        assert_eq!(plain.insert_text, "Grok 4.5");
    }

    #[test]
    fn trailing_space_after_reasoning_model_enters_effort_phase() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);

        let cmd = ModelCommand;
        let ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            workflows_available: true,
            screen_mode: crate::app::ScreenMode::Fullscreen,
        };
        // Args query has a trailing space -> effort phase. Items come out
        // ordered xhigh -> low (strongest first) per EFFORT_LEVELS.
        let items = cmd.suggest_args(&ctx, "Reasoning X ").unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].insert_text, "Reasoning X xhigh");
        assert_eq!(items[1].insert_text, "Reasoning X high");
        assert_eq!(items[2].insert_text, "Reasoning X medium");
        assert_eq!(items[3].insert_text, "Reasoning X low");
        // Display is just the level so the user sees a clean column.
        assert_eq!(items[0].display, "xhigh");
        // match_text carries the sort-key prefix that forces the matcher's
        // alphabetical tiebreak to render rows in EFFORT_LEVELS order.
        assert!(items[0].match_text.starts_with("a "));
        assert!(items[3].match_text.starts_with("d "));
    }

    #[test]
    fn partial_effort_query_still_in_effort_phase() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);

        let cmd = ModelCommand;
        let ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            workflows_available: true,
            screen_mode: crate::app::ScreenMode::Fullscreen,
        };
        // Still in effort phase; matcher upstream narrows to high / xhigh.
        let items = cmd.suggest_args(&ctx, "Reasoning X h").unwrap();
        assert_eq!(items.len(), 4);
    }

    #[test]
    fn partial_model_query_stays_in_model_phase() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);

        let cmd = ModelCommand;
        let ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            workflows_available: true,
            screen_mode: crate::app::ScreenMode::Fullscreen,
        };
        // No trailing space, user is still typing the model name.
        let items = cmd.suggest_args(&ctx, "Reason").unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].insert_text, "Reasoning X ");
    }

    #[test]
    fn run_parses_model_plus_effort_when_supported() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Reasoning X xhigh");
        match result {
            CommandResult::Action(Action::SwitchModel { model_id, effort }) => {
                assert_eq!(model_id.0.as_ref(), "reasoning-x");
                assert_eq!(effort, Some(ReasoningEffort::Xhigh));
            }
            other => panic!("expected SwitchModel with effort, got {other:?}"),
        }
    }

    #[test]
    fn run_rejects_unoffered_effort_with_effort_error_not_unknown_model() {
        // Regression: previously `resolve_effort_token_for` returned None and
        // the handler fell through to `Unknown model: Reasoning X none`.
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Reasoning X none");
        match result {
            CommandResult::Error(msg) => {
                assert!(
                    msg.contains("unknown effort level 'none'"),
                    "expected effort error, got {msg}"
                );
                assert!(
                    msg.contains("use one of:"),
                    "expected offered levels in message, got {msg}"
                );
                assert!(
                    !msg.to_lowercase().contains("unknown model"),
                    "must not misreport as unknown model: {msg}"
                );
                let offered = msg.split_once("; ").map(|(_, r)| r).unwrap_or("");
                assert!(
                    !offered.contains("none"),
                    "must not list none as offered: {msg}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn run_prefers_full_multi_word_model_name_over_prefix_plus_effort() {
        // Catalog has both "Grok" (reasoning) and "Grok 4.5". `/model Grok 4.5`
        // must select the full name, not treat "4.5" as an effort on "Grok".
        let mut state = ModelState::default();
        let (short_id, short_info) = model_with_reasoning("grok", "Grok");
        let (long_id, long_info) = model_with_reasoning("grok-4.5", "Grok 4.5");
        state.available.insert(short_id, short_info);
        state.available.insert(long_id.clone(), long_info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Grok 4.5");
        match result {
            CommandResult::Action(Action::SetDefaultModel(resolved_id)) => {
                assert_eq!(resolved_id, long_id);
            }
            other => panic!("expected SetDefaultModel(Grok 4.5), got {other:?}"),
        }
    }

    #[test]
    fn run_rejects_effort_for_non_reasoning_model() {
        let mut state = ModelState::default();
        let (id, info) = plain_model("grok-4.5", "Grok 4.5");
        state.available.insert(id, info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Grok 4.5 high");
        // Falls through to "is the whole string a model name?" — which
        // it isn't, so we get an Unknown error.
        assert!(matches!(result, CommandResult::Error(_)));
    }

    /// The bare `/model <name>` form dispatches
    /// `Action::SetDefaultModel(<ModelId>)` instead of the legacy
    /// `Action::SwitchModel { effort: None }`. The dispatcher routes
    /// the typed setter through both `Effect::SwitchModel`
    /// (session-level mutation) AND `Effect::PersistSetting`
    /// (next-session default).
    ///
    /// The payload is the typed `acp::ModelId` (resolved at the slash
    /// boundary), not a String.
    #[test]
    fn run_bare_model_name_dispatches_set_default_model() {
        let mut state = ModelState::default();
        let (id, info) = plain_model("grok-4.5", "Grok 4.5");
        state.available.insert(id.clone(), info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Grok 4.5");
        match result {
            CommandResult::Action(Action::SetDefaultModel(resolved_id)) => {
                assert_eq!(resolved_id, id);
            }
            other => panic!("expected Action::SetDefaultModel(<id>), got {other:?}"),
        }
    }

    /// Case-insensitive matching against the catalog: `/model grok 4.5`
    /// resolves to the same `ModelId` as `/model Grok 4.5`.
    #[test]
    fn run_set_default_model_resolves_case_insensitively() {
        let mut state = ModelState::default();
        let (id, info) = plain_model("grok-4.5", "Grok 4.5");
        state.available.insert(id.clone(), info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "grok 4.5");
        match result {
            CommandResult::Action(Action::SetDefaultModel(resolved_id)) => {
                assert_eq!(resolved_id, id);
            }
            other => panic!("expected Action::SetDefaultModel(<id>), got {other:?}"),
        }
    }

    // ── Locked platform models (BYOK discovery) ─────────────────────

    fn locked_platform_model(id: &str, name: &str) -> (acp::ModelId, acp::ModelInfo) {
        let id = acp::ModelId::new(Arc::from(id));
        let meta = serde_json::json!({
            "supportsReasoningEffort": true,
            "requiresApiKey": true,
            "platform": "deepseek",
            "platformName": "DeepSeek",
            "apiKeyEnv": ["GROK_DEEPSEEK_API_KEY", "DEEPSEEK_API_KEY"],
            "setupHint": "export GROK_DEEPSEEK_API_KEY=<key> (or DEEPSEEK_API_KEY), or add `api_key = \"<key>\"` under `[platforms.deepseek]` in ~/.grok/config.toml",
        })
        .as_object()
        .cloned()
        .unwrap();
        let info = acp::ModelInfo::new(id.clone(), name.to_string()).meta(Some(meta));
        (id, info)
    }

    #[test]
    fn locked_models_sort_last_with_lock_marker_and_no_effort_chain() {
        let mut state = ModelState::default();
        let (lid, linfo) = locked_platform_model("deepseek/deepseek-v4-flash", "DeepSeek V4 Flash");
        let (pid, pinfo) = plain_model("grok-4.5", "Grok 4.5");
        state.available.insert(lid, linfo);
        state.available.insert(pid, pinfo);

        let cmd = ModelCommand;
        let ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            screen_mode: crate::app::ScreenMode::Fullscreen,
        };
        let items = cmd.suggest_args(&ctx, "").unwrap();
        assert_eq!(items.len(), 2);
        // Usable model first, locked row last.
        assert_eq!(items[0].match_text, "Grok 4.5");
        assert!(!items[0].locked);
        let locked = &items[1];
        assert!(locked.locked, "locked row must carry the flag for dimming");
        assert!(locked.display.starts_with('🔒'), "lock marker: {locked:?}");
        assert!(locked.display.contains("DeepSeek"), "provider in display");
        // Managed-platform rows insert the catalog id (unique across providers).
        // No trailing space — Enter must submit (→ lock error), never chain
        // into an effort sub-menu, even though the model supports effort.
        assert_eq!(locked.insert_text, "deepseek/deepseek-v4-flash");
        assert!(locked.description.contains("DEEPSEEK_API_KEY"));
    }

    #[test]
    fn locked_reasoning_model_trailing_space_stays_in_model_phase() {
        let mut state = ModelState::default();
        let (lid, linfo) = locked_platform_model("deepseek/deepseek-v4-flash", "DeepSeek V4 Flash");
        state.available.insert(lid, linfo);

        let cmd = ModelCommand;
        let ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            screen_mode: crate::app::ScreenMode::Fullscreen,
        };
        // Trailing space after a locked reasoning-capable model must NOT
        // enter the effort phase — the model list is re-rendered instead.
        let items = cmd.suggest_args(&ctx, "DeepSeek V4 Flash ").unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].locked);
    }

    #[test]
    fn run_locked_model_exact_match_returns_setup_error() {
        let mut state = ModelState::default();
        let (lid, linfo) = locked_platform_model("deepseek/deepseek-v4-flash", "DeepSeek V4 Flash");
        state.available.insert(lid, linfo);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "DeepSeek V4 Flash");
        match result {
            CommandResult::Error(msg) => {
                assert!(msg.contains("DeepSeek"), "provider named: {msg}");
                assert!(msg.contains("DEEPSEEK_API_KEY"), "env var in hint: {msg}");
                assert!(msg.contains("/providers"), "points at /providers: {msg}");
            }
            other => panic!("expected lock Error, got {other:?}"),
        }
    }

    #[test]
    fn run_locked_model_with_effort_token_returns_setup_error() {
        let mut state = ModelState::default();
        let (lid, linfo) = locked_platform_model("deepseek/deepseek-v4-flash", "DeepSeek V4 Flash");
        state.available.insert(lid, linfo);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "DeepSeek V4 Flash high");
        match result {
            CommandResult::Error(msg) => {
                assert!(msg.contains("DEEPSEEK_API_KEY"), "env var in hint: {msg}");
            }
            other => panic!("expected lock Error, got {other:?}"),
        }
    }

    // ── Multi-provider same display name ─────────────────────────────

    fn usable_managed_model(
        id: &str,
        name: &str,
        description: &str,
    ) -> (acp::ModelId, acp::ModelInfo) {
        let id = acp::ModelId::new(Arc::from(id));
        let mut meta = serde_json::Map::new();
        meta.insert(
            "supportsReasoningEffort".into(),
            serde_json::Value::Bool(true),
        );
        let info = acp::ModelInfo::new(id.clone(), name.to_string())
            .description(Some(description.to_string()))
            .meta(serde_json::Value::Object(meta).as_object().cloned());
        (id, info)
    }

    #[test]
    fn multi_provider_same_name_shows_provider_and_unique_insert() {
        let mut state = ModelState::default();
        let (a_id, a_info) = usable_managed_model(
            "zai-coding-cn/glm-5.2",
            "GLM-5.2",
            "Official Pi catalog (zai-coding-cn)",
        );
        let (b_id, b_info) =
            usable_managed_model("ollama/glm-5.2", "GLM-5.2", "Official Pi catalog (ollama)");
        state.available.insert(a_id, a_info);
        state.available.insert(b_id, b_info);

        let cmd = ModelCommand;
        let ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            screen_mode: crate::app::ScreenMode::Fullscreen,
        };
        let items = cmd.suggest_args(&ctx, "").unwrap();
        assert_eq!(items.len(), 2);

        let zai = items
            .iter()
            .find(|i| i.action_id.as_deref() == Some("zai-coding-cn/glm-5.2"))
            .expect("zai row");
        let ollama = items
            .iter()
            .find(|i| i.action_id.as_deref() == Some("ollama/glm-5.2"))
            .expect("ollama row");

        // Left column disambiguates; right column is the human provider name
        // (not the useless "Official Pi catalog (...)" stamp).
        assert!(
            zai.display.contains("Z.AI Coding Plan (CN)"),
            "display: {}",
            zai.display
        );
        assert_eq!(zai.description, "Z.AI Coding Plan (CN)");
        assert_eq!(zai.insert_text, "zai-coding-cn/glm-5.2 ");
        assert!(
            ollama.display.contains("Ollama Cloud"),
            "display: {}",
            ollama.display
        );
        assert_eq!(ollama.description, "Ollama Cloud");
        assert_eq!(ollama.insert_text, "ollama/glm-5.2 ");

        // Type-to-filter can find by provider name.
        assert!(zai.match_text.to_lowercase().contains("coding"));
        assert!(ollama.match_text.to_lowercase().contains("ollama"));
    }

    #[test]
    fn multi_provider_effort_phase_uses_catalog_id_token() {
        let mut state = ModelState::default();
        let (a_id, a_info) = usable_managed_model(
            "zai-coding-cn/glm-5.2",
            "GLM-5.2",
            "Official Pi catalog (zai-coding-cn)",
        );
        let (b_id, b_info) =
            usable_managed_model("ollama/glm-5.2", "GLM-5.2", "Official Pi catalog (ollama)");
        state.available.insert(a_id, a_info);
        state.available.insert(b_id, b_info);

        let cmd = ModelCommand;
        let ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            screen_mode: crate::app::ScreenMode::Fullscreen,
        };
        // Ambiguous bare name + space must NOT enter effort phase.
        let items = cmd.suggest_args(&ctx, "GLM-5.2 ").unwrap();
        assert!(
            items.iter().all(|i| i.action_id.is_some()),
            "must stay in model phase for ambiguous name"
        );

        // Provider-scoped id + space enters effort with that id as the token.
        let items = cmd.suggest_args(&ctx, "zai-coding-cn/glm-5.2 ").unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].insert_text, "zai-coding-cn/glm-5.2 xhigh");
        assert_eq!(items[1].insert_text, "zai-coding-cn/glm-5.2 high");
    }

    #[test]
    fn run_ambiguous_display_name_lists_providers() {
        let mut state = ModelState::default();
        let (a_id, a_info) = usable_managed_model(
            "zai-coding-cn/glm-5.2",
            "GLM-5.2",
            "Official Pi catalog (zai-coding-cn)",
        );
        let (b_id, b_info) =
            usable_managed_model("ollama/glm-5.2", "GLM-5.2", "Official Pi catalog (ollama)");
        state.available.insert(a_id.clone(), a_info);
        state.available.insert(b_id, b_info);

        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "GLM-5.2");
        match result {
            CommandResult::Error(msg) => {
                assert!(msg.contains("multiple providers"), "msg: {msg}");
                assert!(msg.contains("zai-coding-cn/glm-5.2"), "msg: {msg}");
                assert!(msg.contains("ollama/glm-5.2"), "msg: {msg}");
            }
            other => panic!("expected ambiguous Error, got {other:?}"),
        }

        // Explicit catalog id still works.
        let result = ModelCommand.run(&mut ctx, "zai-coding-cn/glm-5.2");
        match result {
            CommandResult::Action(Action::SetDefaultModel(id)) => {
                assert_eq!(id, a_id);
            }
            other => panic!("expected SetDefaultModel, got {other:?}"),
        }
    }

    #[test]
    fn unique_managed_model_shows_provider_description_uses_catalog_id_insert() {
        // Only one GLM-5.2: left stays the friendly name; right shows provider;
        // insert uses catalog id (managed rows always do — stable if another
        // provider later ships the same display name).
        let mut state = ModelState::default();
        let (id, info) = usable_managed_model(
            "zai-coding-cn/glm-5.2",
            "GLM-5.2",
            "Official Pi catalog (zai-coding-cn)",
        );
        state.available.insert(id, info);

        let cmd = ModelCommand;
        let ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            screen_mode: crate::app::ScreenMode::Fullscreen,
        };
        let items = cmd.suggest_args(&ctx, "").unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].display, "GLM-5.2");
        assert_eq!(items[0].description, "Z.AI Coding Plan (CN)");
        assert_eq!(items[0].insert_text, "zai-coding-cn/glm-5.2 ");
    }
}
