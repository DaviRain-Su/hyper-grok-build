pub mod config;
pub mod grok_auth_credentials;
pub mod hooks;
pub(crate) mod subprocess;
pub(crate) mod user_identity;

// The foundation utilities live in `xai-grok-shell-base` (upstream of this
// crate so they build in parallel). Re-exported at the original paths so
// existing `crate::util::…` / `xai_grok_shell::util::…` users compile
// unchanged.
pub use xai_grok_shell_base::util::*;

pub(crate) fn is_user_instruction_path(
    path: &std::path::Path,
    grok_home: &std::path::Path,
    vendor_homes: &[(std::path::PathBuf, bool)],
    workspace_root: Option<&std::path::Path>,
) -> bool {
    let parent = path.parent();
    let grok_rules = grok_home.join("rules");
    let is_exact_home_surface = parent
        .is_some_and(|parent| parent == grok_home || parent == grok_rules)
        || vendor_homes.iter().any(|(vendor_home, named_enabled)| {
            parent.is_some_and(|parent| {
                (*named_enabled && parent == vendor_home) || parent == vendor_home.join("rules")
            })
        });
    if is_exact_home_surface {
        return true;
    }
    if workspace_root.is_some_and(|root| path.starts_with(root)) {
        return false;
    }
    path.starts_with(grok_home)
        || vendor_homes
            .iter()
            .any(|(vendor_home, _)| path.starts_with(vendor_home))
}

/// Aborts the wrapped tokio task when dropped.
///
/// Use to tie a spawned helper task's lifetime to an async scope so that
/// cancelling the parent future (e.g. a turn abort dropping the tool loop)
/// also tears down the helper instead of leaving it running detached.
/// Aborting an already-finished task is a no-op, so this is safe to hold
/// across normal scope exit too.
pub struct AbortOnDrop(pub tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Expand a leading `~` to the home directory; other paths pass through.
pub(crate) fn expand_home(s: &str) -> std::path::PathBuf {
    if let Some(stripped) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    } else if s == "~"
        && let Some(home) = dirs::home_dir()
    {
        return home;
    }
    std::path::PathBuf::from(s)
}

#[cfg(test)]
mod expand_home_tests {
    use super::expand_home;

    #[test]
    fn passthrough_for_absolute_path() {
        assert_eq!(
            expand_home("/abs/path"),
            std::path::PathBuf::from("/abs/path")
        );
    }

    #[test]
    fn passthrough_for_relative_path() {
        assert_eq!(
            expand_home("rel/path"),
            std::path::PathBuf::from("rel/path")
        );
    }

    #[test]
    fn bare_tilde() {
        let home = dirs::home_dir().expect("home_dir required for this test");
        assert_eq!(expand_home("~"), home);
    }

    #[test]
    fn tilde_slash() {
        let home = dirs::home_dir().expect("home_dir required for this test");
        assert_eq!(expand_home("~/foo/bar"), home.join("foo/bar"));
    }

    #[test]
    fn does_not_handle_user_tilde() {
        // `~bob/path` is treated as a literal relative path.
        assert_eq!(
            expand_home("~bob/path"),
            std::path::PathBuf::from("~bob/path")
        );
    }
}

#[cfg(test)]
mod is_user_instruction_path_tests {
    use super::is_user_instruction_path;
    use std::path::Path;

    #[test]
    fn grok_home_named_file_nested_in_workspace_is_user_scoped() {
        assert!(is_user_instruction_path(
            Path::new("/repo/config/AGENTS.md"),
            Path::new("/repo/config"),
            &[],
            Some(Path::new("/repo")),
        ));
        assert!(!is_user_instruction_path(
            Path::new("/repo/config/src/AGENTS.md"),
            Path::new("/repo/config"),
            &[],
            Some(Path::new("/repo")),
        ));
    }

    #[test]
    fn workspace_descendants_under_grok_home_stay_project_scoped() {
        assert!(!is_user_instruction_path(
            Path::new("/custom/grok/worktrees/repo/src/AGENTS.md"),
            Path::new("/custom/grok"),
            &[],
            Some(Path::new("/custom/grok/worktrees/repo")),
        ));
    }
}
