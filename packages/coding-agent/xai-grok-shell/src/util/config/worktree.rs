use super::RemoteSettings;
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
pub(crate) fn worktree_type_from_toml_opt(root: &TomlValue) -> Option<WorktreeType> {
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
pub(crate) fn worktree_type_from_toml(root: &TomlValue) -> WorktreeType {
    worktree_type_from_toml_opt(root).unwrap_or_default()
}

/// Resolve worktree type: local config > remote settings > default (`Linked`).
///
/// Returns the resolved type and its provenance (`"local"`, `"remote"`, or `"default"`).
pub(crate) fn resolve_worktree_type(
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
pub(crate) fn restore_code_from_toml(root: &TomlValue) -> Option<bool> {
    root.get("cli")
        .and_then(|c| c.get("restore_code"))
        .and_then(|v| v.as_bool())
}

/// Resolve restore_code: local config > remote settings > default (`false`).
/// Used when the client omits `restoreCode` on the wire.
pub(crate) fn resolve_restore_code(
    raw_config: &TomlValue,
    remote: Option<&RemoteSettings>,
) -> bool {
    restore_code_from_toml(raw_config)
        .or(remote.and_then(|r| r.restore_code))
        .unwrap_or(false)
}

/// Resolve `[worktree.auto_gc]` from parsed settings: env > local > remote >
/// defaults (clamped). Platform age policy is applied later in `maybe_auto_gc`.
/// (Precedence/clamp behavior is owned and tested in `xai-fast-worktree`'s
/// `resolve_worktree_auto_gc_from_layers`; this only maps settings → layers.)
pub(crate) fn resolve_worktree_auto_gc_from_settings(
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
}
