//! Parse .envrc files and extract environment variables.
//!
//! This module provides a way to load environment variables from `.envrc` files.
//! It uses a two-tier approach:
//!
//! 1. **Try `direnv export json`** - If direnv is installed, use it for full compatibility
//! 2. **Fallback to bash** - Run .envrc in a bash subshell with direnv stubs
//!
//! This approach handles:
//! - Variable expansion ($HOME, ${VAR:-default})
//! - Command substitution ($(git rev-parse ...))
//! - Conditional logic (if/then/else)
//! - direnv helper functions (source_up_if_exists, PATH_add, etc.)

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

/// Stub implementations of common direnv helper functions.
/// These are prepended to the .envrc before execution when direnv is not available.
const DIRENV_STUBS: &str = r#"
# Stub direnv helper functions
source_up_if_exists() { :; }
source_up() { :; }
source_env_if_exists() {
    if [ -f "$1" ]; then
        . "$1"
    fi
}
source_env() {
    if [ -f "$1" ]; then
        . "$1"
    fi
}
PATH_add() {
    export PATH="$PWD/$1:$PATH"
}
path_add() {
    PATH_add "$@"
}
layout() { :; }
use() { :; }
watch_file() { :; }
"#;

/// Load environment variables from .envrc file in the given directory.
///
/// Returns a HashMap of environment variables that were set/modified by the .envrc.
/// Returns None if no .envrc exists or if parsing fails.
pub fn load_envrc(dir: &Path) -> Option<HashMap<String, String>> {
    let envrc_path = dir.join(".envrc");
    if !envrc_path.exists() {
        tracing::debug!(?dir, ".envrc not found");
        return None;
    }

    // Try direnv first (most reliable if installed)
    if let Some(env) = try_direnv_export(dir) {
        return Some(env);
    }

    // Fall back to bash subshell approach
    load_envrc_via_bash(dir)
}

/// Try to use `direnv export json` to load environment variables.
/// Returns None if direnv is not installed or fails.
fn try_direnv_export(dir: &Path) -> Option<HashMap<String, String>> {
    let mut cmd = Command::new("direnv");
    cmd.args(["export", "json"])
        .current_dir(dir)
        .stdin(std::process::Stdio::null());
    xai_grok_tools::util::detach_std_command(&mut cmd);
    let output = cmd.output().ok()?;

    if !output.status.success() {
        // direnv not allowed, or other error
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains("not allowed") {
            tracing::debug!(?dir, %stderr, "direnv export failed");
        }
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        // No changes from direnv
        return None;
    }

    // Parse JSON output: {"VAR": "value", ...}
    // Note: direnv also outputs null for vars to unset, but we ignore those
    match serde_json::from_str::<HashMap<String, serde_json::Value>>(&stdout) {
        Ok(json) => {
            let env: HashMap<String, String> = json
                .into_iter()
                .filter_map(|(k, v)| {
                    if let serde_json::Value::String(s) = v {
                        Some((k, s))
                    } else {
                        None // Skip null values (unset)
                    }
                })
                .collect();

            if env.is_empty() {
                None
            } else {
                tracing::info!(?dir, count = env.len(), "Loaded environment via direnv");
                Some(env)
            }
        }
        Err(e) => {
            tracing::warn!(?dir, ?e, "Failed to parse direnv JSON output");
            None
        }
    }
}

/// Load environment by running .envrc in a bash subshell.
/// This is the fallback when direnv is not installed.
fn load_envrc_via_bash(dir: &Path) -> Option<HashMap<String, String>> {
    let envrc_path = dir.join(".envrc");

    // Build a script that:
    // 1. Includes direnv stubs
    // 2. Sources the .envrc
    // 3. Outputs all env vars as KEY=VALUE pairs (null-separated for safety)
    let script = format!(
        r#"
set -e
cd "{dir}"
{stubs}
. "{envrc}"
# Output all environment variables, null-separated
env -0
"#,
        dir = dir.display(),
        stubs = DIRENV_STUBS,
        envrc = envrc_path.display(),
    );

    // Capture baseline environment (before running .envrc)
    let baseline: HashMap<String, String> = std::env::vars().collect();

    // Run the script and capture output
    let mut bash_cmd = Command::new("/bin/bash");
    bash_cmd
        .arg("-c")
        .arg(&script)
        .current_dir(dir)
        .stdin(std::process::Stdio::null());
    xai_grok_tools::util::detach_std_command(&mut bash_cmd);
    let output = bash_cmd.output();

    let output = match output {
        Ok(o) if o.status.success() => o,
        Ok(_) => {
            tracing::warn!(?envrc_path, "Failed to execute .envrc via bash");
            return None;
        }
        Err(e) => {
            tracing::warn!(?envrc_path, ?e, "Failed to run bash for .envrc");
            return None;
        }
    };

    // Parse the null-separated KEY=VALUE pairs
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut result: HashMap<String, String> = HashMap::new();

    for entry in stdout.split('\0') {
        if entry.is_empty() {
            continue;
        }
        if let Some((key, value)) = entry.split_once('=') {
            // Skip internal/noise variables
            let ignored_keys = ["_", "SHLVL", "PWD", "OLDPWD"];
            if ignored_keys.contains(&key) {
                continue;
            }
            // Only include vars that are new or changed from baseline
            match baseline.get(key) {
                Some(baseline_value) if baseline_value == value => {
                    // Unchanged, skip
                }
                _ => {
                    // New or changed
                    result.insert(key.to_string(), value.to_string());
                }
            }
        }
    }

    if result.is_empty() {
        tracing::debug!(?envrc_path, "No environment changes from .envrc");
        None
    } else {
        tracing::info!(
            ?envrc_path,
            count = result.len(),
            "Loaded environment from .envrc via bash"
        );
        Some(result)
    }
}

/// Load .envrc and return the environment, or empty HashMap on failure.
pub fn load_envrc_or_empty(dir: &Path) -> HashMap<String, String> {
    load_envrc(dir).unwrap_or_default()
}

/// [`load_envrc_or_empty`] gated on folder-trust: loads the repo-local `.envrc`
/// (executed in a bash subshell) only when `trusted`, else returns an empty map.
/// The shell call sites pass the `project_scope_allowed` verdict so the "run a
/// cloned repo's `.envrc` only when the folder is trusted" rule lives in ONE
/// place (mirrors `permission::claude_settings::load_claude_env_with_project`).
pub fn load_envrc_or_empty_when_trusted(dir: &Path, trusted: bool) -> HashMap<String, String> {
    if trusted {
        load_envrc_or_empty(dir)
    } else {
        HashMap::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_simple_export() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(".envrc"), "export FOO=bar\n").unwrap();

        let env = load_envrc(dir.path()).unwrap();
        assert_eq!(env.get("FOO"), Some(&"bar".to_string()));
    }

    #[test]
    fn test_variable_expansion() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(".envrc"), "export MY_DIR=$PWD/subdir\n").unwrap();

        let env = load_envrc(dir.path()).unwrap();
        let expected = format!("{}/subdir", dir.path().display());
        assert_eq!(env.get("MY_DIR"), Some(&expected));
    }

    #[test]
    fn test_no_envrc() {
        let dir = TempDir::new().unwrap();
        assert!(load_envrc(dir.path()).is_none());
    }

    #[test]
    fn test_path_add() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(".envrc"), "PATH_add bin\n").unwrap();

        let env = load_envrc(dir.path()).unwrap();
        let path = env.get("PATH").unwrap();
        assert!(path.contains(&format!("{}/bin", dir.path().display())));
    }

    #[test]
    fn test_conditional() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(".envrc"),
            r#"
if [ -d "$PWD" ]; then
    export EXISTS=yes
else
    export EXISTS=no
fi
"#,
        )
        .unwrap();

        let env = load_envrc(dir.path()).unwrap();
        assert_eq!(env.get("EXISTS"), Some(&"yes".to_string()));
    }
}
