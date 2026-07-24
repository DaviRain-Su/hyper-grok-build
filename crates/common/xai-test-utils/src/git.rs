//! Hermetic git helpers for tests.
//!
//! When running under `bazel test`, the `GIT_BIN_PATH` environment variable
//! points to a statically-linked git binary provided by Bazel.  The helpers
//! in this module prepend that binary's directory to `PATH` so that
//! `Command::new("git")` resolves to it instead of relying on a
//! system-installed git.

use std::path::{Path, PathBuf};
use std::sync::Once;

static HERMETIC_GIT_INIT: Once = Once::new();

/// Prepend the hermetic git binary directory to `PATH` so that
/// `Command::new("git")` resolves to the Bazel-provided static binary
/// instead of relying on a system-installed git.
///
/// Safe to call multiple times — only the first call mutates `PATH`.
pub fn ensure_hermetic_git_on_path() {
    HERMETIC_GIT_INIT.call_once(|| {
        if let Ok(git_bin) = std::env::var("GIT_BIN_PATH") {
            let git_path = PathBuf::from(&git_bin);
            let git_path = if git_path.is_relative() {
                std::env::current_dir().unwrap().join(&git_path)
            } else {
                git_path
            };
            if let Some(bin_dir) = git_path.parent() {
                let current_path = std::env::var("PATH").unwrap_or_default();
                // SAFETY: called once via `Once` before any child processes are spawned.
                unsafe {
                    std::env::set_var("PATH", format!("{}:{}", bin_dir.display(), current_path));
                    // git-minimal spawns subcommands (`git stash` → `git
                    // update-index`) through its exec path, which is baked to
                    // a build-machine prefix. Helpers live next to the binary,
                    // so point the exec path there. Skip the host-fallback
                    // wrapper (`git-host-fallback.sh`): host git must keep its
                    // own exec path.
                    if git_path.file_name().is_some_and(|name| name == "git") {
                        std::env::set_var("GIT_EXEC_PATH", bin_dir);
                    }
                }
            }
        }
    });
}

/// Ensure the hermetic git binary is on `PATH` before running tests that
/// need git.  Call at the top of any `#[test]` that spawns `git` commands.
///
/// ```ignore
/// #[test]
/// fn my_git_test() {
///     xai_test_utils::require_git!();
///     // ... git commands work here ...
/// }
/// ```
#[macro_export]
macro_rules! require_git {
    () => {
        $crate::git::ensure_hermetic_git_on_path();
    };
}

/// Initialise a fresh git repository at `path` with a dummy user config.
///
/// Calls [`ensure_hermetic_git_on_path`] first so the hermetic binary is used.
pub fn init_git_repo(path: &Path) {
    ensure_hermetic_git_on_path();
    std::process::Command::new("git")
        .current_dir(path)
        .args(["init"])
        .output()
        .unwrap();

    std::process::Command::new("git")
        .current_dir(path)
        .args(["config", "user.email", "test@test.com"])
        .output()
        .unwrap();

    std::process::Command::new("git")
        .current_dir(path)
        .args(["config", "user.name", "Test"])
        .output()
        .unwrap();
}

/// Stage all files and create a commit.
///
/// Calls [`ensure_hermetic_git_on_path`] first so the hermetic binary is used.
pub fn git_commit_all(path: &Path, message: &str) {
    ensure_hermetic_git_on_path();
    std::process::Command::new("git")
        .current_dir(path)
        .args(["add", "."])
        .output()
        .unwrap();
    std::process::Command::new("git")
        .current_dir(path)
        .args(["commit", "-m", message])
        .output()
        .unwrap();
}

/// Run a git command in `dir` with a deterministic author/committer, assert
/// success, and return trimmed stdout.
///
/// Calls [`ensure_hermetic_git_on_path`] first so the hermetic binary is used.
pub fn run_git(dir: &Path, args: &[&str]) -> String {
    run_git_with_env(dir, args, &[])
}

/// Like [`run_git`], with extra environment variables (e.g.
/// `GIT_SEQUENCE_EDITOR`). Hermetic beyond the binary and author identity:
/// the developer's global/system git config is masked (a local
/// `commit.gpgsign`/`core.hooksPath`/`rebase.autoSquash` must not change
/// test behavior) and credential prompts are disabled. `envs` is applied
/// last, so callers can override any of this.
pub fn run_git_with_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> String {
    ensure_hermetic_git_on_path();
    let mut cmd = std::process::Command::new("git");
    cmd.args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@test.com")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Write a grouped fan-out tree of ~`files` files (`files_per_dir` per
/// directory, directories bucketed 100 per group) under `dir`. No git
/// operations — callers stage/commit as needed.
pub fn write_fanout_tree(dir: &Path, files: usize, files_per_dir: usize) {
    for d in 0..files.div_ceil(files_per_dir) {
        let sub = dir.join(format!("g{}", d / 100)).join(format!("d{d}"));
        std::fs::create_dir_all(&sub).expect("create populated dir");
        for f in 0..files_per_dir {
            std::fs::write(
                sub.join(format!("file_{f}.txt")),
                format!("content {d} {f}\n"),
            )
            .expect("write populated file");
        }
    }
}

/// Create a `feature` branch with `picks` one-file commits off the current
/// HEAD, advance the base branch by one commit (so a rebase has work), and
/// leave `feature` checked out. Returns the base branch name.
pub fn make_feature_branch(dir: &Path, picks: usize) -> String {
    let base = run_git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
    run_git(dir, &["checkout", "-b", "feature"]);
    for k in 0..picks {
        let name = format!("pick_{k}.txt");
        std::fs::write(dir.join(&name), format!("pick {k}\n")).expect("write pick file");
        run_git(dir, &["add", &name]);
        run_git(dir, &["commit", "-m", &format!("pick {k}")]);
    }
    run_git(dir, &["checkout", &base]);
    std::fs::write(dir.join("base_advance.txt"), "advance\n").expect("write base advance file");
    run_git(dir, &["add", "base_advance.txt"]);
    run_git(dir, &["commit", "-m", "advance base"]);
    run_git(dir, &["checkout", "feature"]);
    base
}
