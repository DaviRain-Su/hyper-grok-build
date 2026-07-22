//! Agent bootstrap and lifecycle hooks.
//!
//! [`bootstrap`] runs the full init sequence (config resolution, process
//! singletons, model catalog) and returns a resolved config + `ModelsManager`.
//! [`update_telemetry_config`] re-initializes telemetry after auth changes.

use std::sync::Arc;

use indexmap::IndexMap;

use crate::agent::config::{self, Config as AgentConfig, ModelEntry};
use crate::agent::models::ModelsManager;
use crate::auth::AuthManager;
use crate::config::StorageMode;

/// Resolve config, init process singletons, build the model catalog.
///
/// The `ModelsManager` is `Clone + Send`, so callers that need a handle
/// for the config watcher can clone it before passing it to
/// `MvpAgent::with_models`.
pub fn bootstrap(
    cfg: &AgentConfig,
    auth_manager: &Arc<AuthManager>,
    prefetched: Option<IndexMap<String, ModelEntry>>,
) -> Result<(AgentConfig, ModelsManager), String> {
    // Fail closed before any policy is read: a tampered managed policy must not run unmanaged.
    crate::managed_config::managed_policy_gate()?;
    let cfg = resolve_config(cfg, auth_manager);
    cfg.validate_model_filters()?;
    init_process(&cfg, auth_manager);
    let models_manager = ModelsManager::from_config(&cfg, prefetched, auth_manager.clone())?;

    // Refresh on every auth refresh — the FSEvents watcher can silently die after
    // macOS sleep, stranding the catalog on bundled defaults.
    models_manager.start_auth_refresh_watcher(auth_manager.refresh_notifier());

    Ok((cfg, models_manager))
}

/// Print a `bootstrap`/`MvpAgent::new` config error and exit (process boundary).
///
/// Restores native stderr first: a managed-policy refusal on the ACP/server path reaches here
/// while fd 2 may still point at the `/dev/null` the TUI's `redirect_native_stderr()` set, which
/// would swallow the message. No-op when stderr was never redirected (headless).
pub(crate) fn exit_on_config_error<T>(e: String) -> T {
    xai_tty_utils::restore_native_stderr();
    eprintln!("\nConfiguration error:\n\n    {e}\n");
    std::process::exit(1);
}

/// Config transform: apply managed settings, fetch remote settings,
/// resolve storage mode.
fn resolve_config(cfg: &AgentConfig, auth_manager: &AuthManager) -> AgentConfig {
    let mut cfg = cfg.clone();

    if let Ok(layers) = crate::config::ConfigLayers::load()
        && layers.has_managed()
    {
        let origins = crate::config::config_origins(&layers);
        let managed_keys: Vec<&str> = origins
            .iter()
            .filter(|(_, s)| matches!(s, config::ConfigSource::ManagedConfig))
            .map(|(k, _)| k.as_str())
            .collect();
        if !managed_keys.is_empty() {
            tracing::info!(keys = ?managed_keys, "managed_config.toml fields");
        }
    }

    let managed_enforced = crate::config::apply_managed_settings_features(&mut cfg);
    let requirements_enforced = crate::config::apply_requirements(&mut cfg);

    for e in managed_enforced.iter().chain(&requirements_enforced) {
        tracing::info!(field = %e.path, value = %e.value, source = %e.source, "policy override");
    }

    // Fallback: if the client didn't pre-supply remote settings, fetch them
    // now so remote-settings-gated features work regardless of which client
    // spawned us.  Clients that already call `start_early_prefetch()` and
    // thread the result into `cfg.remote_settings` skip this entirely.
    if cfg.remote_settings.is_none()
        && let Some(handle) =
            crate::agent::models::start_early_prefetch(Some(cfg.grok_com_config.clone()))
    {
        match handle.join() {
            Ok(result) => {
                cfg.remote_settings = result.settings;
                crate::util::config::set_remote_campaigns_from_settings(
                    cfg.remote_settings.as_ref(),
                );
                tracing::info!("remote_settings fetched as shell-level fallback");
            }
            Err(_) => {
                tracing::warn!("remote_settings fallback prefetch thread panicked");
            }
        }
    }
    crate::util::config::sync_campaign_fields(&mut cfg);
    crate::agent::config::apply_remote_settings_side_effects(cfg.remote_settings.as_ref());

    // env var > remote settings > Local. Skip remote settings for Generic (grok -p, subagents).
    if cfg.storage_mode == StorageMode::Local
        && cfg.mode != crate::agent::config::AgentMode::Generic
    {
        cfg.storage_mode = StorageMode::resolve(None, cfg.remote_settings.as_ref());
    }
    // Writeback talks to the code backend; requires grok.com auth.
    if cfg.storage_mode == StorageMode::Writeback
        && !auth_manager.current().is_some_and(|a| a.is_xai_auth())
    {
        tracing::info!("Writeback is disabled: requires auth with grok.com");
        cfg.storage_mode = StorageMode::Local;
    }

    if let Some(rs) = cfg.remote_settings.as_ref()
        && let Some(v) = rs.path_not_found_hints
    {
        cfg.path_not_found_hints = v;
    }

    cfg
}

/// Initialize process-level singletons (deployment sync, built-in metadata,
/// telemetry). `Once`-guarded: only the first call takes effect.
/// Telemetry user ID is updated separately via [`update_telemetry_config`].
fn init_process(cfg: &AgentConfig, auth_manager: &AuthManager) {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        if !cfg!(test) {
            // Clear a logged-out team's files before the background sync runs.
            crate::managed_config::clear_orphan();
            crate::managed_config::spawn_sync(tokio_util::sync::CancellationToken::new());
        }

        let grok_home = crate::util::grok_home::grok_home();
        crate::builtin::extract_builtin_files(&grok_home);

        crate::extensions::marketplace::purge_default_skills_installs(&grok_home);

        // Auto-register is gated (default off; env/remote settings enables). Kept out
        // of built-in extraction so the gate can read the resolved
        // remote_settings, which resolve_config has populated by now.
        if cfg.resolve_official_marketplace_auto_register().value {
            crate::extensions::marketplace::ensure_official_marketplace_source(&grok_home);
        }

        let telemetry_mode = cfg.resolve_telemetry_mode();
        let trace_upload = cfg.resolve_trace_upload();
        let feedback = cfg.resolve_feedback();
        let feedback_url = cfg.endpoints.resolve_feedback_base_url();
        let trace_upload_url = cfg.endpoints.resolve_trace_upload_url();
        tracing::info!(
            telemetry = %telemetry_mode,
            trace_upload = %trace_upload,
            feedback = %feedback,
            feedback_url = %feedback_url,
            feedback_url_custom = cfg.endpoints.feedback_base_url.is_some(),
            trace_upload_url = %trace_upload_url,
            trace_upload_url_custom = cfg.endpoints.trace_upload_url.is_some(),
            trace_upload_bucket = cfg.endpoints.trace_upload_bucket.as_deref().unwrap_or("none"),
            trace_upload_region = cfg.endpoints.trace_upload_region.as_deref().unwrap_or("none"),
            "data capture config resolved",
        );
        if telemetry_mode.value.is_disabled() && trace_upload.value {
            tracing::info!(
                "Telemetry disabled but trace uploads enabled: \
                 session artifacts will be uploaded, analytics events will not"
            );
        }
        update_telemetry_config(cfg, auth_manager);
    });
}

/// Apply current telemetry config + auth identity. Tears down the client
/// when telemetry is disabled, so it's safe to call repeatedly.
pub fn update_telemetry_config(config: &AgentConfig, auth_manager: &AuthManager) {
    let grok_auth = auth_manager.current().filter(|a| a.is_xai_auth());
    let user_id = grok_auth.as_ref().map(|a| a.user_id.clone());
    let team_id = grok_auth.as_ref().and_then(|a| a.team_id.clone());
    let subscription_tier = super::mvp_agent::resolve_subscription_tier_for_telemetry(
        config
            .remote_settings
            .as_ref()
            .and_then(|rs| rs.subscription_tier_display.clone()),
        auth_manager.current_or_expired().as_ref(),
    );
    xai_grok_telemetry::client::init(
        config.telemetry.clone(),
        config.resolve_telemetry_mode().value,
        user_id,
        team_id,
        config.endpoints.deployment_key.clone(),
        crate::http::origin_client_info_from_env(),
        xai_grok_version::VERSION.to_owned(),
        subscription_tier,
        crate::http::shared_client(),
    );
}
