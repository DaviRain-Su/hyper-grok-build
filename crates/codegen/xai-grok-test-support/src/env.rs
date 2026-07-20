//! Shared environment helpers: binary resolution, git workdirs, env var setup.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// RAII guard for a single environment variable in `#[serial]` tests: snapshots
/// the prior value on construction, applies the change, then restores the prior
/// value (or unsets it) on drop — even if an assertion panics. Restoring rather
/// than always unsetting avoids clobbering vars a parent process/harness set
/// (e.g. `RUST_LOG`).
///
/// Callers MUST be `#[serial_test::serial]`: the `unsafe` `set_var`/`remove_var`
/// are sound only when no other thread accesses the environment concurrently.
pub struct EnvGuard {
    key: &'static str,
    prior: Option<OsString>,
}

impl EnvGuard {
    /// Set `key` to `value` for the guard's lifetime. Accepts `&str`, `&Path`,
    /// `String`, etc. via `AsRef<OsStr>`.
    pub fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let prior = std::env::var_os(key);
        // SAFETY: callers are `#[serial]`, so no other thread touches the env.
        unsafe { std::env::set_var(key, value) };
        Self { key, prior }
    }

    /// Unset `key` for the guard's lifetime.
    pub fn unset(key: &'static str) -> Self {
        let prior = std::env::var_os(key);
        // SAFETY: see [`EnvGuard::set`].
        unsafe { std::env::remove_var(key) };
        Self { key, prior }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: see [`EnvGuard::set`].
        match self.prior.take() {
            Some(v) => unsafe { std::env::set_var(self.key, v) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

fn workspace_root() -> PathBuf {
    // nth(3): crate is nested three levels below the cargo workspace root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("workspace root")
        .to_path_buf()
}

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("target"))
}

fn local_grok_binary_path() -> PathBuf {
    target_dir()
        .join("debug")
        .join(format!("xai-grok-pager{}", std::env::consts::EXE_SUFFIX))
}

fn ensure_local_grok_binary(binary: &Path) {
    if binary.exists() {
        return;
    }

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(&cargo)
        .current_dir(workspace_root())
        .args([
            "build",
            "-p",
            "xai-grok-pager-bin",
            "--bin",
            "xai-grok-pager",
        ])
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {cargo} to build xai-grok-pager: {e}"));

    assert!(
        output.status.success(),
        "failed to build xai-grok-pager for lifecycle tests (exit {:?})\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        binary.exists(),
        "xai-grok-pager build completed but binary missing at {}",
        binary.display()
    );
}

/// Resolve grok binary: `GROK_BINARY` env (CI) or a locally built `xai-grok-pager` binary.
pub fn grok_binary() -> PathBuf {
    if let Ok(path) = std::env::var("GROK_BINARY") {
        let p = PathBuf::from(path);
        assert!(p.exists(), "GROK_BINARY does not exist: {}", p.display());
        return p;
    }

    if let Ok(path) = std::env::var("CARGO_BIN_EXE_xai-grok-pager") {
        let p = PathBuf::from(path);
        if p.exists() {
            return p;
        }
    }

    let binary = local_grok_binary_path();
    ensure_local_grok_binary(&binary);
    binary
}

/// Temp dir with a git repo + one committed file.
/// Forces libgit2 to fully init (the codepath that breaks with bad OpenSSL linking).
pub fn git_workdir() -> TempDir {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path();

    fn run_git(args: &[&str], dir: &Path) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn git {}: {e}", args.join(" ")));
        assert!(
            output.status.success(),
            "git {} failed (exit {:?}):\n{}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    run_git(&["init"], path);
    // Configure git user for commits (required in CI where no global config exists)
    run_git(&["config", "user.email", "test@test.com"], path);
    run_git(&["config", "user.name", "Test"], path);

    std::fs::write(path.join("README.md"), "test file\n").expect("write test file");

    run_git(&["add", "-A"], path);
    run_git(&["commit", "-m", "init", "--no-gpg-sign"], path);

    dir
}

/// Point grok at the mock server with a fake API key and telemetry disabled.
pub fn test_env_cmd_tokio(
    cmd: &mut tokio::process::Command,
    mock_url: &str,
    home: &std::path::Path,
) {
    cmd.env("HOME", home)
        // HOME alone does not sandbox grok on Windows: the product resolves
        // `~` via `USERPROFILE`/Known Folders (`std::env::home_dir()`), so
        // without an explicit GROK_HOME every spawned child shares the real
        // `%USERPROFILE%\.grok` — test 1's models_cache.json (which embeds
        // its per-test mock-server URL) then poisons every later test's
        // prompt (the windows-x86_64 lifecycle "prompt timed out" failure).
        // Mirrors `leader.rs` and the pty-harness `env_for_pager`.
        .env("GROK_HOME", home.join(".grok"))
        .env("GROK_CLI_CHAT_PROXY_BASE_URL", mock_url)
        .env("GROK_XAI_API_BASE_URL", mock_url)
        .env("XAI_API_KEY", "test-key-for-ci")
        .env("GROK_TELEMETRY_ENABLED", "false")
        .env("GROK_FEEDBACK_ENABLED", "false")
        .env("GROK_TRACE_UPLOAD", "false")
        .env("GROK_INSTRUMENTATION", "disabled")
        // Release binaries (CI lifecycle tests) otherwise spawn a background
        // update check that hits the network and can add latency under Rosetta.
        .env("GROK_DISABLE_AUTOUPDATER", "1");
}
