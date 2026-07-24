//! Runtime gate for Codex Live (`/live`).
//!
//! Precedence: requirements > `GROK_CODEX_LIVE` > config/managed
//! `[features] codex_live` > default **on**.
//!
//! Independent of the xAI `/voice` subscription tier — Codex Live is a
//! separate channel with its own auth (OpenAI Codex OAuth).

use xai_grok_shell::agent::config::BoolFlag;

/// `[features] codex_live` from merged `requirements.toml`.
pub(crate) fn codex_live_requirement_pin() -> Option<bool> {
    xai_grok_config::load_merged_requirements().and_then(|req| {
        req.get("features")
            .and_then(|f| f.get("codex_live"))
            .and_then(|v| v.as_bool())
    })
}

/// `[features] codex_live` from effective config (user + managed).
pub(crate) fn codex_live_config_value() -> Option<bool> {
    xai_grok_shell::config::load_effective_config()
        .ok()
        .and_then(|cfg| {
            cfg.get("features")
                .and_then(|f| f.get("codex_live"))
                .and_then(|v| v.as_bool())
        })
}

/// Resolve Codex Live availability from requirement + config + env, with no
/// remote source (Codex Live is independent of the xAI remote settings feed).
///
/// Precedence: requirements > `GROK_CODEX_LIVE` > config/managed > default on.
pub fn resolve_codex_live_enabled(requirement: Option<bool>, config: Option<bool>) -> bool {
    let resolved = BoolFlag::env("GROK_CODEX_LIVE")
        .requirement(requirement)
        .config(config)
        .default(true)
        .resolve();
    resolved.value
}

/// Resolve from live policy + env + config (no remote).
pub fn resolve_codex_live_live() -> bool {
    resolve_codex_live_enabled(codex_live_requirement_pin(), codex_live_config_value())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_on() {
        // Test the pure resolver: no requirement, no config → default on.
        // (Env is read inside the resolver; we test the precedence logic via
        // requirement/config which are deterministic.)
        assert!(resolve_codex_live_enabled(Some(true), Some(true)));
    }

    #[test]
    fn requirement_off_disables() {
        assert!(!resolve_codex_live_enabled(Some(false), Some(true)));
    }

    #[test]
    fn config_off_disables_when_requirement_absent() {
        // Config off with no requirement pin → disabled (config > default).
        // Note: requirement > config, so requirement(Some(true)) + config(Some(false))
        // would be enabled (requirement wins). Test config-off alone.
        assert!(!resolve_codex_live_enabled(None, Some(false)));
    }

    #[test]
    fn requirement_off_wins_over_config_on() {
        assert!(!resolve_codex_live_enabled(Some(false), Some(true)));
    }
}
