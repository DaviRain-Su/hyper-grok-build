use super::RemoteSettings;
use super::mcp::use_leader_from_toml;
use serde::{Deserialize, Serialize};
use toml::Value as TomlValue;
use xai_fast_worktree::CreationMode;

/// Worktree creation type configuration.
///
/// Mirrors the internal `CreationMode` enum from xai-fast-worktree but uses
/// config-friendly naming (lowercase strings in TOML).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WorktreeType {
    /// Linked worktree via `git worktree add --no-checkout` + parallel CoW copy.
    /// This is the fastest mode for large repos.
    #[default]
    Linked,
    /// Standalone repository copy with independent `.git/` directory.
    /// Can be promoted to replace the source via `rename()`.
    Standalone,
    /// Plain `git worktree add` with full checkout.
    Git,
}

impl std::str::FromStr for WorktreeType {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "linked" => Ok(Self::Linked),
            "standalone" => Ok(Self::Standalone),
            "git" => Ok(Self::Git),
            _ => Err(()),
        }
    }
}

impl From<WorktreeType> for CreationMode {
    fn from(t: WorktreeType) -> Self {
        match t {
            WorktreeType::Linked => CreationMode::Linked,
            WorktreeType::Standalone => CreationMode::Standalone,
            WorktreeType::Git => CreationMode::GitCheckout,
        }
    }
}

/// Returns `Some(type)` when `[cli] worktree_type` is set to a valid value in config.toml,
/// `None` when absent or the value type is wrong. Logs a warning for invalid strings.
pub fn worktree_type_from_toml_opt(root: &TomlValue) -> Option<WorktreeType> {
    if let TomlValue::Table(table) = root
        && let Some(TomlValue::Table(cli)) = table.get("cli")
        && let Some(toml_value) = cli.get("worktree_type")
    {
        if let Some(type_str) = toml_value.as_str() {
            return match type_str.parse::<WorktreeType>() {
                Ok(wt) => Some(wt),
                Err(()) => {
                    tracing::warn!("Invalid worktree_type value in config: {type_str}, ignoring");
                    None
                }
            };
        }
        tracing::warn!("Invalid worktree_type value in config: {toml_value:?}, ignoring");
    }
    None
}

/// Get the worktree type from config.toml.
///
/// Set in config.toml under [cli] as `worktree_type = "linked|standalone|git"`.
/// Defaults to `WorktreeType::Linked` when not explicitly set.
pub fn worktree_type_from_toml(root: &TomlValue) -> WorktreeType {
    worktree_type_from_toml_opt(root).unwrap_or_default()
}

/// Resolve worktree type: local config > remote settings > default (`Linked`).
///
/// Returns the resolved type and its provenance (`"local"`, `"remote"`, or `"default"`).
pub fn resolve_worktree_type(
    raw_config: &TomlValue,
    remote: Option<&RemoteSettings>,
) -> (WorktreeType, &'static str) {
    if let Some(wt) = worktree_type_from_toml_opt(raw_config) {
        return (wt, "local");
    }
    if let Some(s) = remote.and_then(|r| r.worktree_type.as_deref()) {
        match s.parse::<WorktreeType>() {
            Ok(wt) => return (wt, "remote"),
            Err(()) => {
                tracing::warn!("Invalid remote worktree_type: {s}, using default");
            }
        }
    }
    (WorktreeType::default(), "default")
}

/// Synchronously get the worktree type from the config file.
pub fn worktree_type() -> WorktreeType {
    let root: TomlValue = match crate::config::load_effective_config() {
        Ok(r) => r,
        Err(_) => return WorktreeType::Linked,
    };
    worktree_type_from_toml(&root)
}

/// Returns `Some(value)` when `[cli] restore_code` is set as a boolean in config.toml.
pub fn restore_code_from_toml(root: &TomlValue) -> Option<bool> {
    root.get("cli")
        .and_then(|c| c.get("restore_code"))
        .and_then(|v| v.as_bool())
}

/// Resolve restore_code: local config > remote settings > default (`false`).
pub fn resolve_restore_code(raw_config: &TomlValue, remote: Option<&RemoteSettings>) -> bool {
    restore_code_from_toml(raw_config)
        .or(remote.and_then(|r| r.restore_code))
        .unwrap_or(false)
}

/// Synchronously check if leader mode is enabled in the config file.
/// When true, the agent will connect to a shared leader process instead of
/// running the agent directly. This allows multiple agent instances to share one backend.
/// Defaults to false when not explicitly set.
pub fn use_leader_sync() -> bool {
    let root: TomlValue = match crate::config::load_effective_config() {
        Ok(r) => r,
        Err(_) => return false,
    };
    use_leader_from_toml(&root)
}

/// Parse `[worktree.auto_gc]` (per-field tolerant via [`WorktreeAutoGcSettings`]).
pub fn worktree_auto_gc_from_toml(root: &TomlValue) -> super::WorktreeAutoGcSettings {
    root.get("worktree")
        .and_then(|w| w.get("auto_gc"))
        // toml::Value only deserializes by value (no &Value Deserializer).
        .and_then(|v| super::WorktreeAutoGcSettings::deserialize(v.clone()).ok())
        .unwrap_or_default()
}

/// Resolve: env > local TOML > remote > defaults (clamped). Platform age policy
/// is applied later in `maybe_auto_gc`.
pub fn resolve_worktree_auto_gc(
    raw_config: &TomlValue,
    remote: Option<&RemoteSettings>,
) -> xai_fast_worktree::ResolvedWorktreeAutoGc {
    let local = worktree_auto_gc_from_toml(raw_config);
    resolve_worktree_auto_gc_from_settings(
        Some(&local),
        remote.and_then(|r| r.worktree_auto_gc.as_ref()),
    )
}

/// Same layering with already-parsed settings.
pub fn resolve_worktree_auto_gc_from_settings(
    local: Option<&super::WorktreeAutoGcSettings>,
    remote: Option<&super::WorktreeAutoGcSettings>,
) -> xai_fast_worktree::ResolvedWorktreeAutoGc {
    use xai_grok_workspace::worktree::worktree_auto_gc_layer_from_settings;
    let local_layer = local.map(worktree_auto_gc_layer_from_settings);
    let remote_layer = remote.map(worktree_auto_gc_layer_from_settings);
    xai_fast_worktree::resolve_worktree_auto_gc_from_layers(
        local_layer.as_ref(),
        remote_layer.as_ref(),
    )
}

#[cfg(test)]
mod tests {
    use super::RemoteSettings;
    use super::*;
    use toml::Value as TomlValue;

    #[test]
    fn test_worktree_type_linked() {
        let toml_str = r#"
[cli]
worktree_type = "linked"
"#;
        let root: TomlValue = toml::from_str(toml_str).unwrap();
        assert_eq!(worktree_type_from_toml(&root), WorktreeType::Linked);
    }

    #[test]
    fn test_worktree_type_standalone() {
        let toml_str = r#"
[cli]
worktree_type = "standalone"
"#;
        let root: TomlValue = toml::from_str(toml_str).unwrap();
        assert_eq!(worktree_type_from_toml(&root), WorktreeType::Standalone);
    }

    #[test]
    fn test_worktree_type_git() {
        let toml_str = r#"
[cli]
worktree_type = "git"
"#;
        let root: TomlValue = toml::from_str(toml_str).unwrap();
        assert_eq!(worktree_type_from_toml(&root), WorktreeType::Git);
    }

    #[test]
    fn test_worktree_type_default_linked() {
        let toml_str = r#"
[cli]
auto_update = true
"#;
        let root: TomlValue = toml::from_str(toml_str).unwrap();
        assert_eq!(worktree_type_from_toml(&root), WorktreeType::Linked);
    }

    #[test]
    fn test_worktree_type_no_cli_section() {
        let toml_str = r#"
[models]
default = "grok-code-fast-1"
"#;
        let root: TomlValue = toml::from_str(toml_str).unwrap();
        assert_eq!(worktree_type_from_toml(&root), WorktreeType::Linked);
    }

    #[test]
    fn test_worktree_type_invalid_value() {
        let toml_str = r#"
[cli]
worktree_type = "invalid"
"#;
        let root: TomlValue = toml::from_str(toml_str).unwrap();
        // Invalid values should fall back to default
        assert_eq!(worktree_type_from_toml(&root), WorktreeType::Linked);
    }

    #[test]
    fn test_worktree_type_fromstr() {
        assert_eq!("linked".parse::<WorktreeType>(), Ok(WorktreeType::Linked));
        assert_eq!(
            "standalone".parse::<WorktreeType>(),
            Ok(WorktreeType::Standalone)
        );
        assert_eq!("git".parse::<WorktreeType>(), Ok(WorktreeType::Git));
        assert!("invalid".parse::<WorktreeType>().is_err());
        assert!("LINKED".parse::<WorktreeType>().is_err());
    }

    #[test]
    fn test_worktree_type_from_toml_opt_present() {
        let root: TomlValue = toml::from_str("[cli]\nworktree_type = \"standalone\"").unwrap();
        assert_eq!(
            worktree_type_from_toml_opt(&root),
            Some(WorktreeType::Standalone)
        );
    }

    #[test]
    fn test_worktree_type_from_toml_opt_absent() {
        let root: TomlValue = toml::from_str("[cli]\nauto_update = true").unwrap();
        assert_eq!(worktree_type_from_toml_opt(&root), None);
    }

    #[test]
    fn test_worktree_type_from_toml_opt_invalid() {
        let root: TomlValue = toml::from_str("[cli]\nworktree_type = \"bogus\"").unwrap();
        assert_eq!(worktree_type_from_toml_opt(&root), None);
    }

    #[test]
    fn test_worktree_type_from_toml_opt_no_cli_section() {
        let root: TomlValue = toml::from_str("[models]\ndefault = \"grok\"").unwrap();
        assert_eq!(worktree_type_from_toml_opt(&root), None);
    }

    #[test]
    fn test_resolve_worktree_type_local_wins_over_remote() {
        let root: TomlValue = toml::from_str("[cli]\nworktree_type = \"git\"").unwrap();
        let remote = RemoteSettings {
            worktree_type: Some("standalone".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            resolve_worktree_type(&root, Some(&remote)),
            (WorktreeType::Git, "local")
        );
    }

    #[test]
    fn test_resolve_worktree_type_remote_fallback() {
        let root: TomlValue = toml::from_str("[cli]\nauto_update = true").unwrap();
        let remote = RemoteSettings {
            worktree_type: Some("standalone".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            resolve_worktree_type(&root, Some(&remote)),
            (WorktreeType::Standalone, "remote")
        );
    }

    #[test]
    fn test_resolve_worktree_type_default_when_no_config() {
        let root: TomlValue = toml::from_str("[cli]\nauto_update = true").unwrap();
        assert_eq!(
            resolve_worktree_type(&root, None),
            (WorktreeType::Linked, "default")
        );
    }

    #[test]
    fn test_resolve_worktree_type_invalid_remote_falls_back_to_default() {
        let root: TomlValue = toml::from_str("[cli]\nauto_update = true").unwrap();
        let remote = RemoteSettings {
            worktree_type: Some("bogus".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            resolve_worktree_type(&root, Some(&remote)),
            (WorktreeType::Linked, "default")
        );
    }

    #[test]
    fn test_resolve_worktree_type_remote_none_field() {
        let root: TomlValue = toml::from_str("[cli]\nauto_update = true").unwrap();
        let remote = RemoteSettings {
            worktree_type: None,
            ..Default::default()
        };
        assert_eq!(
            resolve_worktree_type(&root, Some(&remote)),
            (WorktreeType::Linked, "default")
        );
    }

    // === restore_code config tests ===

    #[test]
    fn test_restore_code_from_toml_present_true() {
        let root: TomlValue = toml::from_str("[cli]\nrestore_code = true").unwrap();
        assert_eq!(restore_code_from_toml(&root), Some(true));
    }

    #[test]
    fn test_restore_code_from_toml_present_false() {
        let root: TomlValue = toml::from_str("[cli]\nrestore_code = false").unwrap();
        assert_eq!(restore_code_from_toml(&root), Some(false));
    }

    #[test]
    fn test_restore_code_from_toml_absent() {
        let root: TomlValue = toml::from_str("[cli]\nauto_update = true").unwrap();
        assert_eq!(restore_code_from_toml(&root), None);
    }

    #[test]
    fn test_restore_code_from_toml_no_cli_section() {
        let root: TomlValue = toml::from_str("[models]\ndefault = \"grok\"").unwrap();
        assert_eq!(restore_code_from_toml(&root), None);
    }

    #[test]
    fn test_restore_code_from_toml_wrong_type() {
        let root: TomlValue = toml::from_str("[cli]\nrestore_code = \"yes\"").unwrap();
        assert_eq!(restore_code_from_toml(&root), None);
    }

    #[test]
    fn test_resolve_restore_code_local_wins_over_remote() {
        let root: TomlValue = toml::from_str("[cli]\nrestore_code = true").unwrap();
        let remote = RemoteSettings {
            restore_code: Some(false),
            ..Default::default()
        };
        assert!(resolve_restore_code(&root, Some(&remote)));
    }

    #[test]
    fn test_resolve_restore_code_remote_fallback() {
        let root: TomlValue = toml::from_str("[cli]\nauto_update = true").unwrap();
        let remote = RemoteSettings {
            restore_code: Some(true),
            ..Default::default()
        };
        assert!(resolve_restore_code(&root, Some(&remote)));
    }

    #[test]
    fn test_resolve_restore_code_default_false() {
        let root: TomlValue = toml::from_str("[cli]\nauto_update = true").unwrap();
        assert!(!resolve_restore_code(&root, None));
    }

    #[test]
    fn test_resolve_restore_code_remote_none_falls_to_default() {
        let root: TomlValue = toml::from_str("[cli]\nauto_update = true").unwrap();
        let remote = RemoteSettings {
            restore_code: None,
            ..Default::default()
        };
        assert!(!resolve_restore_code(&root, Some(&remote)));
    }

    #[test]
    fn test_resolve_restore_code_local_false_overrides_remote_true() {
        let root: TomlValue = toml::from_str("[cli]\nrestore_code = false").unwrap();
        let remote = RemoteSettings {
            restore_code: Some(true),
            ..Default::default()
        };
        assert!(!resolve_restore_code(&root, Some(&remote)));
    }

    // === worktree auto_gc resolve tests ===

    use crate::util::config::WorktreeAutoGcSettings;
    use serial_test::serial;

    fn clear_auto_gc_env() {
        unsafe {
            std::env::remove_var(xai_fast_worktree::ENV_AUTO_GC);
            std::env::remove_var(xai_fast_worktree::ENV_AUTO_GC_DRY_RUN);
            std::env::remove_var(xai_fast_worktree::ENV_AUTO_GC_MAX_AGE);
            std::env::remove_var(xai_fast_worktree::ENV_AUTO_GC_REBUILD);
        }
    }

    #[test]
    #[serial]
    fn resolve_worktree_auto_gc_defaults_enabled() {
        clear_auto_gc_env();
        let root: TomlValue = toml::from_str("").unwrap();
        let p = resolve_worktree_auto_gc(&root, None);
        assert!(p.enabled);
        assert_eq!(p.max_age_secs, xai_fast_worktree::DEFAULT_MAX_AGE_SECS);
        assert_eq!(
            p.min_interval_secs,
            xai_fast_worktree::DEFAULT_MIN_INTERVAL_SECS
        );
        assert!(!p.dry_run);
        assert_eq!(
            p.include_orphan_snapshots,
            cfg!(target_os = "linux"),
            "orphan default is platform-gated"
        );
        assert!(!p.include_rebuild, "rebuild off by default");
        assert_eq!(
            p.rebuild_min_interval_secs,
            xai_fast_worktree::DEFAULT_REBUILD_MIN_INTERVAL_SECS
        );
    }

    #[test]
    #[serial]
    fn resolve_worktree_auto_gc_rebuild_env_and_toml() {
        clear_auto_gc_env();
        unsafe { std::env::set_var(xai_fast_worktree::ENV_AUTO_GC_REBUILD, "1") };
        let root: TomlValue = toml::from_str("").unwrap();
        let p = resolve_worktree_auto_gc(&root, None);
        assert!(p.include_rebuild, "env REBUILD=1 enables rebuild");
        clear_auto_gc_env();

        let root: TomlValue = toml::from_str(
            r#"
            [worktree.auto_gc]
            include_rebuild = true
            rebuild_min_interval_secs = 120
            "#,
        )
        .unwrap();
        let p = resolve_worktree_auto_gc(&root, None);
        assert!(p.include_rebuild);
        assert_eq!(p.rebuild_min_interval_secs, 120);
    }

    #[test]
    #[serial]
    fn resolve_worktree_auto_gc_env_disabled_wins_over_remote_and_local() {
        clear_auto_gc_env();
        unsafe { std::env::set_var(xai_fast_worktree::ENV_AUTO_GC, "0") };
        let root: TomlValue = toml::from_str(
            r#"
            [worktree.auto_gc]
            enabled = true
            "#,
        )
        .unwrap();
        let remote = RemoteSettings {
            worktree_auto_gc: Some(WorktreeAutoGcSettings {
                enabled: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let p = resolve_worktree_auto_gc(&root, Some(&remote));
        assert!(!p.enabled, "env kill must win over remote/local enabled");
        clear_auto_gc_env();
    }

    #[test]
    #[serial]
    fn resolve_worktree_auto_gc_remote_enabled_false_disables() {
        clear_auto_gc_env();
        let root: TomlValue = toml::from_str("").unwrap();
        let remote = RemoteSettings {
            worktree_auto_gc: Some(WorktreeAutoGcSettings {
                enabled: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };
        let p = resolve_worktree_auto_gc(&root, Some(&remote));
        assert!(!p.enabled);
    }

    #[test]
    #[serial]
    fn resolve_worktree_auto_gc_local_wins_over_remote_ttl_and_env_dry_run_wins() {
        clear_auto_gc_env();
        unsafe { std::env::set_var(xai_fast_worktree::ENV_AUTO_GC_DRY_RUN, "1") };
        let root: TomlValue = toml::from_str(
            r#"
            [worktree.auto_gc]
            max_age_secs = 7200
            min_interval_secs = 120
            dry_run = false
            "#,
        )
        .unwrap();
        let remote = RemoteSettings {
            worktree_auto_gc: Some(WorktreeAutoGcSettings {
                max_age_secs: Some(86400),
                min_interval_secs: Some(3600),
                dry_run: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };
        let p = resolve_worktree_auto_gc(&root, Some(&remote));
        assert_eq!(p.max_age_secs, 7200, "local TOML wins over remote TTL");
        assert_eq!(p.min_interval_secs, 120, "local interval wins");
        assert!(
            p.dry_run,
            "env dry-run wins over remote/local dry_run=false"
        );
        clear_auto_gc_env();
    }

    #[test]
    #[serial]
    fn resolve_worktree_auto_gc_remote_ttl_clamped() {
        clear_auto_gc_env();
        let root: TomlValue = toml::from_str("").unwrap();
        let remote = RemoteSettings {
            worktree_auto_gc: Some(WorktreeAutoGcSettings {
                max_age_secs: Some(1),
                min_interval_secs: Some(1),
                ..Default::default()
            }),
            ..Default::default()
        };
        let p = resolve_worktree_auto_gc(&root, Some(&remote));
        assert_eq!(p.max_age_secs, xai_fast_worktree::MAX_AGE_SECS_MIN);
        assert_eq!(
            p.min_interval_secs,
            xai_fast_worktree::MIN_INTERVAL_SECS_MIN
        );
    }

    #[test]
    #[serial]
    fn resolve_worktree_auto_gc_partial_remote_falls_through() {
        clear_auto_gc_env();
        let root: TomlValue = toml::from_str("").unwrap();
        let remote = RemoteSettings {
            worktree_auto_gc: Some(WorktreeAutoGcSettings {
                enabled: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let p = resolve_worktree_auto_gc(&root, Some(&remote));
        assert!(p.enabled);
        assert_eq!(p.max_age_secs, xai_fast_worktree::DEFAULT_MAX_AGE_SECS);
        assert_eq!(
            p.min_interval_secs,
            xai_fast_worktree::DEFAULT_MIN_INTERVAL_SECS
        );
    }

    #[test]
    #[serial]
    fn resolve_worktree_auto_gc_local_enabled_false_over_remote_true() {
        clear_auto_gc_env();
        let root: TomlValue = toml::from_str(
            r#"
            [worktree.auto_gc]
            enabled = false
            "#,
        )
        .unwrap();
        let remote = RemoteSettings {
            worktree_auto_gc: Some(WorktreeAutoGcSettings {
                enabled: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let p = resolve_worktree_auto_gc(&root, Some(&remote));
        assert!(!p.enabled, "local enabled=false must beat remote true");
    }

    #[test]
    #[serial]
    fn resolve_worktree_auto_gc_local_dry_run_over_remote() {
        clear_auto_gc_env();
        let root: TomlValue = toml::from_str(
            r#"
            [worktree.auto_gc]
            dry_run = true
            "#,
        )
        .unwrap();
        let remote = RemoteSettings {
            worktree_auto_gc: Some(WorktreeAutoGcSettings {
                dry_run: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };
        let p = resolve_worktree_auto_gc(&root, Some(&remote));
        assert!(p.dry_run, "local dry_run must beat remote false");
    }

    #[test]
    #[serial]
    fn resolve_worktree_auto_gc_upper_clamp_via_resolve() {
        clear_auto_gc_env();
        let root: TomlValue = toml::from_str(
            r#"
            [worktree.auto_gc]
            max_age_secs = 999999999
            min_interval_secs = 999999999
            "#,
        )
        .unwrap();
        let p = resolve_worktree_auto_gc(&root, None);
        assert_eq!(p.max_age_secs, xai_fast_worktree::MAX_AGE_SECS_MAX);
        assert_eq!(
            p.min_interval_secs,
            xai_fast_worktree::MIN_INTERVAL_SECS_MAX
        );
    }

    #[test]
    #[serial]
    fn worktree_auto_gc_toml_bad_field_keeps_enabled_false() {
        // Typo/wrong type next to enabled=false must not drop the kill-switch.
        clear_auto_gc_env();
        let root: TomlValue = toml::from_str(
            r#"
            [worktree.auto_gc]
            enabled = false
            max_age_secs = "not-a-number"
            "#,
        )
        .unwrap();
        let s = worktree_auto_gc_from_toml(&root);
        assert_eq!(s.enabled, Some(false));
        assert_eq!(s.max_age_secs, None);
        let p = resolve_worktree_auto_gc(&root, None);
        assert!(!p.enabled);
    }

    #[test]
    #[serial]
    fn resolve_worktree_auto_gc_defaults_manual_never() {
        clear_auto_gc_env();
        let root: TomlValue = toml::from_str("").unwrap();
        let p = resolve_worktree_auto_gc(&root, None);
        assert_eq!(
            p.max_age_by_kind
                .get(&xai_fast_worktree::WorktreeKind::Manual),
            Some(&None),
            "product default: manual never age-expires"
        );
        assert!(
            !p.max_age_by_kind
                .contains_key(&xai_fast_worktree::WorktreeKind::Session),
            "session uses default max_age_secs, not an explicit map entry"
        );
    }

    #[test]
    #[serial]
    fn resolve_worktree_auto_gc_kind_map_local_wins_over_remote() {
        clear_auto_gc_env();
        let root: TomlValue = toml::from_str(
            r#"
            [worktree.auto_gc.max_age_by_kind]
            subagent = 7200
            manual = "never"
            "#,
        )
        .unwrap();
        let remote = RemoteSettings {
            worktree_auto_gc: Some(WorktreeAutoGcSettings {
                max_age_by_kind: Some(
                    [
                        (
                            "subagent".into(),
                            crate::util::config::WorktreeKindMaxAge::Secs(86400),
                        ),
                        (
                            "manual".into(),
                            crate::util::config::WorktreeKindMaxAge::Secs(3600),
                        ),
                        (
                            "pool".into(),
                            crate::util::config::WorktreeKindMaxAge::Secs(172800),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                ),
                ..Default::default()
            }),
            ..Default::default()
        };
        let p = resolve_worktree_auto_gc(&root, Some(&remote));
        use xai_fast_worktree::WorktreeKind;
        assert_eq!(
            p.max_age_by_kind.get(&WorktreeKind::Subagent),
            Some(&Some(7200)),
            "local kind TTL wins"
        );
        assert_eq!(
            p.max_age_by_kind.get(&WorktreeKind::Manual),
            Some(&None),
            "local manual=never wins over remote expire"
        );
        assert_eq!(
            p.max_age_by_kind.get(&WorktreeKind::Pool),
            Some(&Some(172800)),
            "remote-only kind entry kept"
        );
    }

    #[test]
    #[serial]
    fn resolve_worktree_auto_gc_env_max_age() {
        clear_auto_gc_env();
        unsafe { std::env::set_var(xai_fast_worktree::ENV_AUTO_GC_MAX_AGE, "7200") };
        let root: TomlValue = toml::from_str(
            r#"
            [worktree.auto_gc]
            max_age_secs = 86400
            "#,
        )
        .unwrap();
        let p = resolve_worktree_auto_gc(&root, None);
        assert_eq!(p.max_age_secs, 7200, "env MAX_AGE wins over TOML");
        clear_auto_gc_env();
    }

    #[test]
    #[serial]
    fn worktree_auto_gc_toml_kind_map_parses_never() {
        clear_auto_gc_env();
        let root: TomlValue = toml::from_str(
            r#"
            [worktree.auto_gc]
            max_age_secs = 604800
            [worktree.auto_gc.max_age_by_kind]
            subagent = 86400
            manual = "never"
            "#,
        )
        .unwrap();
        let s = worktree_auto_gc_from_toml(&root);
        let map = s.max_age_by_kind.as_ref().expect("kind map present");
        assert_eq!(
            map.get("subagent"),
            Some(&crate::util::config::WorktreeKindMaxAge::Secs(86400))
        );
        assert_eq!(
            map.get("manual"),
            Some(&crate::util::config::WorktreeKindMaxAge::Never)
        );
        let p = resolve_worktree_auto_gc(&root, None);
        assert_eq!(
            p.max_age_by_kind
                .get(&xai_fast_worktree::WorktreeKind::Subagent),
            Some(&Some(86400))
        );
        assert_eq!(
            p.max_age_by_kind
                .get(&xai_fast_worktree::WorktreeKind::Manual),
            Some(&None)
        );
    }
}
