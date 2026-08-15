//! Session glue for the scheme live extension runtime (fourth dispatch
//! segment: shell hooks → client hooks → wasm → scheme).
//!
//! The runtime itself lives in `xai-grok-scheme-runtime`; this module owns the
//! host-side policy: where live state lives (`~/.grok/live/`), where prebuilt
//! image binaries are expected (`~/.grok/bin/hyper-scheme-image`), the
//! `[live] disabled = true` escape hatch, and spec collection from the plugin
//! registry (trusted + enabled plugins with `runtime.scheme` only).

use xai_grok_scheme_runtime::{SchemeRuntime, SchemeRuntimeConfig};

/// `~/.grok/live/` — journal + kernel cache root.
pub(crate) fn live_state_dir() -> std::path::PathBuf {
    xai_grok_config::grok_home().join("live")
}

/// Prebuilt image binary locations, best first (Phase 4 distribution target).
fn prebuilt_image_candidates() -> Vec<std::path::PathBuf> {
    let mut out = vec![
        xai_grok_config::grok_home()
            .join("bin")
            .join("hyper-scheme-image"),
    ];
    // Alongside the current executable (bundled installs).
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        out.push(dir.join("hyper-scheme-image"));
    }
    out
}

/// Escape hatch: `[live] disabled = true` in config, or `GROK_LIVE_DISABLED=1`.
pub(crate) fn live_runtime_disabled() -> bool {
    if std::env::var("GROK_LIVE_DISABLED")
        .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    {
        return true;
    }
    crate::config::load_effective_config()
        .ok()
        .and_then(|t| {
            t.get("live")
                .and_then(|l| l.get("disabled"))
                .and_then(|d| d.as_bool())
        })
        .unwrap_or(false)
}

/// Production per-session runtime (empty until [`rebuild_for_session`]).
pub(crate) fn new_session_scheme_runtime() -> SchemeRuntime {
    SchemeRuntime::new(SchemeRuntimeConfig {
        state_dir: live_state_dir(),
        prebuilt_candidates: prebuilt_image_candidates(),
        allow_path_discovery: true,
    })
}

/// Inert runtime for test session actors: no plugins, no PATH discovery.
#[cfg(test)]
pub(crate) fn inert_for_tests() -> SchemeRuntime {
    SchemeRuntime::new(SchemeRuntimeConfig {
        state_dir: std::env::temp_dir().join("grok-live-test"),
        prebuilt_candidates: Vec::new(),
        allow_path_discovery: false,
    })
}

/// Collect scheme specs from active (trusted + enabled) plugins. Empty when
/// the live runtime is disabled by config.
pub(crate) fn scheme_specs_from_registry(
    registry: Option<&xai_grok_agent::plugins::PluginRegistry>,
) -> Vec<xai_grok_extension_api::SchemeSpec> {
    if live_runtime_disabled() {
        return Vec::new();
    }
    registry
        .map(|reg| {
            reg.active_plugins()
                .into_iter()
                .filter_map(|p| p.scheme_spec())
                .collect()
        })
        .unwrap_or_default()
}

/// Rebuild the session's scheme runtime from the registry (spawn + reload).
/// The image itself boots lazily on the first dispatch.
pub(crate) async fn rebuild_for_session(
    runtime: &SchemeRuntime,
    registry: Option<&xai_grok_agent::plugins::PluginRegistry>,
    session_id: &str,
) {
    let specs = scheme_specs_from_registry(registry);
    let had_any = !runtime.is_empty();
    if specs.is_empty() && !had_any {
        return;
    }
    runtime.rebuild_from_specs(specs).await;
    tracing::info!(
        target: "scheme_extension",
        session_id = %session_id,
        scheme_extensions = runtime.len(),
        "scheme runtime rebuilt from plugin registry"
    );
}

/// Log observe-dispatch failures (always fail-open).
pub(crate) fn log_observe_failures(event: &str, results: &[xai_grok_scheme_runtime::SchemeCallResult]) {
    for r in results {
        match r {
            xai_grok_scheme_runtime::SchemeCallResult::Failed { plugin, error }
            | xai_grok_scheme_runtime::SchemeCallResult::HandlerError { plugin, error } => {
                tracing::debug!(
                    target: "scheme_extension",
                    plugin = %plugin,
                    event = %event,
                    error = %error,
                    "scheme extension observe failed (fail-open)"
                );
            }
            _ => {}
        }
    }
}
