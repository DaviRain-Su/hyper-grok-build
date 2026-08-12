//! On-demand model catalog refresh for `/model`.

use std::collections::HashSet;
use std::sync::Arc;

use agent_client_protocol as acp;
use indexmap::IndexMap;

use crate::acp::model_state::ModelState;
use crate::app::app_view::AppView;

use xai_grok_shell::agent::config::{CONFIG_MODEL_META_KEY, Config};
use xai_grok_shell::agent::models::ModelsManager;

struct ConfigModelProjection {
    available: IndexMap<acp::ModelId, acp::ModelInfo>,
    config_ids: HashSet<acp::ModelId>,
    fallback_current: Option<acp::ModelId>,
}

impl ConfigModelProjection {
    fn load() -> Result<Self, String> {
        let raw = xai_grok_shell::config::load_effective_config()
            .map_err(|error| format!("failed to load config.toml: {error}"))?;
        let config = Config::new_from_toml_cfg(&raw)
            .map_err(|error| format!("failed to parse config.toml: {error}"))?;
        config.validate_model_filters()?;

        let config_ids = config
            .config_models
            .keys()
            .cloned()
            .map(acp::ModelId::new)
            .collect();
        let auth_manager = Arc::new(config.create_auth_manager());
        let manager = ModelsManager::from_config(&config, None, auth_manager)
            .map_err(|error| format!("failed to resolve config.toml models: {error}"))?;

        Ok(Self {
            available: manager.available(),
            config_ids,
            fallback_current: Some(manager.current_model_id()),
        })
    }

    fn apply(&self, state: &ModelState) -> ModelState {
        if !state.reload_config_on_model_command {
            return state.clone();
        }

        let old_config_ids = state
            .available
            .iter()
            .filter(|(_, info)| is_config_model(info))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let mut available = state.available.clone();
        available.retain(|_, info| !is_config_model(info));

        // A removed override can reveal an underlying cached/default model
        // with the same key. Restore that donor when the fresh projection has
        // one; otherwise the removed config-only row disappears.
        for id in old_config_ids {
            if !self.config_ids.contains(&id)
                && let Some(info) = self.available.get(&id)
            {
                available.insert(id, info.clone());
            }
        }

        // Current config rows replace same-key remote/default entries.
        for id in &self.config_ids {
            if let Some(info) = self.available.get(id) {
                available.insert(id.clone(), info.clone());
            }
        }

        let fallback = state
            .current
            .as_ref()
            .filter(|id| available.contains_key(*id))
            .cloned()
            .or_else(|| {
                self.fallback_current
                    .as_ref()
                    .filter(|id| available.contains_key(*id))
                    .cloned()
            })
            .or_else(|| available.first().map(|(id, _)| id.clone()));

        let mut refreshed = state.clone();
        refreshed.update_catalog_with_fallback(available, fallback);
        refreshed
    }
}

fn is_config_model(info: &acp::ModelInfo) -> bool {
    info.meta
        .as_ref()
        .and_then(|meta| meta.get(CONFIG_MODEL_META_KEY))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Reload model definitions for one slash-command context. This remains pure
/// from the caller's perspective, which lets autocomplete preview fresh rows
/// before the app-wide catalog is committed on command execution.
pub(crate) fn refreshed_model_state(state: &ModelState) -> Result<ModelState, String> {
    if !state.reload_config_on_model_command {
        return Ok(state.clone());
    }
    Ok(ConfigModelProjection::load()?.apply(state))
}

/// Commit one disk snapshot to every pager model catalog so `/model` command
/// validation, the dashboard, and all live sessions observe the same rows.
pub(crate) fn refresh_app_model_catalog(app: &mut AppView) -> Result<(), String> {
    let reload_enabled = app.models.reload_config_on_model_command
        || app
            .agents
            .values()
            .any(|agent| agent.session.models.reload_config_on_model_command);
    if !reload_enabled {
        return Ok(());
    }

    let projection = ConfigModelProjection::load()?;
    app.models = projection.apply(&app.models);
    for agent in app.agents.values_mut() {
        agent.session.models = projection.apply(&agent.session.models);
    }
    if let Some(dashboard) = app.dashboard.as_mut() {
        dashboard.models = projection.apply(&dashboard.models);
        if dashboard
            .pending_model
            .as_ref()
            .is_some_and(|pending| !dashboard.models.available.contains_key(&pending.id))
        {
            dashboard.pending_model = None;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: &str, configured: bool) -> acp::ModelInfo {
        let id = acp::ModelId::new(id);
        let meta = configured.then(|| {
            let mut meta = serde_json::Map::new();
            meta.insert(CONFIG_MODEL_META_KEY.to_string(), true.into());
            meta
        });
        acp::ModelInfo::new(id, "Model".to_string()).meta(meta)
    }

    #[test]
    fn merge_replaces_only_config_owned_rows() {
        let old = acp::ModelId::new("old-config");
        let remote = acp::ModelId::new("remote");
        let codex = acp::ModelId::new("codex/gpt-5");
        let new = acp::ModelId::new("new-config");

        let mut state = ModelState::default();
        state.reload_config_on_model_command = true;
        state
            .available
            .insert(old.clone(), info("old-config", true));
        state
            .available
            .insert(remote.clone(), info("remote", false));
        state
            .available
            .insert(codex.clone(), info("codex/gpt-5", false));
        state.current = Some(remote.clone());

        let projection = ConfigModelProjection {
            available: IndexMap::from([(new.clone(), info("new-config", true))]),
            config_ids: HashSet::from([new.clone()]),
            fallback_current: Some(new.clone()),
        };
        let refreshed = projection.apply(&state);

        assert!(!refreshed.available.contains_key(&old));
        assert!(refreshed.available.contains_key(&new));
        assert!(refreshed.available.contains_key(&remote));
        assert!(refreshed.available.contains_key(&codex));
        assert_eq!(refreshed.current, Some(remote));
    }

    #[test]
    fn removed_override_restores_fresh_donor_and_keeps_current() {
        let id = acp::ModelId::new("shared");
        let mut state = ModelState::default();
        state.reload_config_on_model_command = true;
        state.available.insert(id.clone(), info("shared", true));
        state.current = Some(id.clone());

        let projection = ConfigModelProjection {
            available: IndexMap::from([(id.clone(), info("shared", false))]),
            config_ids: HashSet::new(),
            fallback_current: Some(id.clone()),
        };
        let refreshed = projection.apply(&state);

        assert!(refreshed.available.contains_key(&id));
        assert!(!is_config_model(&refreshed.available[&id]));
        assert_eq!(refreshed.current, Some(id));
    }
}
