//! Git marketplace source support.
//!
//! Provides persistent caching of git marketplace repos.
//! Cache root: `~/.grok/marketplace-cache/<url-hash>/`

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use fs2::FileExt;
use wait_timeout::ChildExt;

/// Default TTL for marketplace cache freshness (5 minutes).
const CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Hard cap for clone/fetch so a bad marketplace URL cannot hang list/refresh.
const NETWORK_OP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    UseTtl,
    Force,
}

pub struct SourceCacheLease {
    pub path: PathBuf,
    lock_file: File,
}

impl Drop for SourceCacheLease {
    fn drop(&mut self) {
        let _ = self.lock_file.unlock();
    }
}

/// Sync a git marketplace source to the persistent cache.
///
/// Returns the path to the cached repo on success.
pub fn sync_source_cache(
    url: &str,
    branch: Option<&str>,
    cache_root: &Path,
) -> Result<PathBuf, String> {
    let lease = sync_source_cache_with_mode(url, branch, cache_root, SyncMode::UseTtl)?;
    Ok(lease.path.clone())
}

pub fn force_sync_source_cache(
    url: &str,
    branch: Option<&str>,
    cache_root: &Path,
) -> Result<PathBuf, String> {
    let lease = sync_source_cache_with_mode(url, branch, cache_root, SyncMode::Force)?;
    Ok(lease.path.clone())
}

pub fn sync_source_cache_with_mode(
    url: &str,
    branch: Option<&str>,
    cache_root: &Path,
    mode: SyncMode,
) -> Result<SourceCacheLease, String> {
    let url = xai_grok_agent::plugins::git_install::validate_git_url(url)?;
    let branch = branch
        .map(xai_grok_agent::plugins::git_install::validate_git_ref)
        .transpose()?;
    let hash = cache_hash(url);
    let cache_dir = cache_root.join(&hash);
    let start = Instant::now();

    std::fs::create_dir_all(cache_root).map_err(|e| format!("failed to create cache root: {e}"))?;
    let lock_file = acquire_cache_lock(&cache_root.join(format!("{hash}.lock")), LOCK_TIMEOUT)?;

    let result = sync_cache_locked(url, branch, &cache_dir, mode);
    match &result {
        Ok(()) => {
            tracing::debug!(mode = ?mode, elapsed_ms = start.elapsed().as_millis(), "marketplace cache sync complete")
        }
        Err(error) => {
            tracing::warn!(mode = ?mode, elapsed_ms = start.elapsed().as_millis(), error = %error, "marketplace cache sync failed")
        }
    }
    result?;

    Ok(SourceCacheLease {
        path: cache_dir,
        lock_file,
    })
}

fn sync_cache_locked(
    url: &str,
    branch: Option<&str>,
    cache_dir: &Path,
    mode: SyncMode,
) -> Result<(), String> {
    let url = xai_grok_agent::plugins::git_install::validate_git_url(url)?;
    let branch = branch
        .map(xai_grok_agent::plugins::git_install::validate_git_ref)
        .transpose()?;
    if cache_dir.join(".git").exists() {
        if mode == SyncMode::UseTtl && is_cache_fresh(cache_dir) {
            return Ok(());
        }
        fetch_reset_cached_repo(cache_dir, branch).or_else(|e| {
            tracing::warn!(error = %e, "git fetch/reset failed, re-cloning marketplace cache");
            reclone_repo(url, branch, cache_dir)
        })
    } else {
        clone_repo(url, branch, cache_dir)
    }
}

fn acquire_cache_lock(lock_path: &Path, timeout: Duration) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .map_err(|e| format!("failed to open cache lock {}: {e}", lock_path.display()))?;
    let deadline = Instant::now() + timeout;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "cache lock timeout after {}s for {}",
                        timeout.as_secs(),
                        lock_path.display()
                    ));
                }
                std::thread::sleep(LOCK_POLL_INTERVAL);
            }
            Err(e) => return Err(format!("failed to lock cache {}: {e}", lock_path.display())),
        }
    }
}

/// Check if the cache was fetched recently enough to skip fetching.
fn is_cache_fresh(cache_dir: &Path) -> bool {
    let fetch_head = cache_dir.join(".git").join("FETCH_HEAD");
    match std::fs::metadata(&fetch_head) {
        Ok(meta) => meta
            .modified()
            .ok()
            .and_then(|mtime| mtime.elapsed().ok())
            .is_some_and(|age| age < CACHE_TTL),
        Err(_) => false,
    }
}

/// Get the default cache root directory.
pub fn default_cache_root() -> PathBuf {
    xai_grok_config::grok_home().join("marketplace-cache")
}

/// Deterministic hash for a URL (used as cache directory name).
fn cache_hash(url: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Clone a git repo with depth 1.
///
/// Uses the git CLI (not libgit2): a libgit2 clone cannot be killed on
/// timeout, so a hung remote would pin a thread forever.
fn clone_repo(url: &str, branch: Option<&str>, dest: &Path) -> Result<(), String> {
    let url = xai_grok_agent::plugins::git_install::validate_git_url(url)?;
    let branch = branch
        .map(xai_grok_agent::plugins::git_install::validate_git_ref)
        .transpose()?;
    let mut cmd = clone_cli_command(url, branch, dest);
    run_git_timed(&mut cmd, "clone", NETWORK_OP_TIMEOUT).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(dest);
    })
}

fn reclone_repo(url: &str, branch: Option<&str>, dest: &Path) -> Result<(), String> {
    let parent = dest
        .parent()
        .ok_or_else(|| format!("cache path has no parent: {}", dest.display()))?;
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("cache path has no file name: {}", dest.display()))?;
    let suffix = format!("{}-{}", std::process::id(), unique_reclone_suffix());
    let temp_dest = parent.join(format!(".{name}.reclone-{suffix}"));
    let backup_dest = parent.join(format!(".{name}.backup-{suffix}"));

    let _ = std::fs::remove_dir_all(&temp_dest);
    let _ = std::fs::remove_dir_all(&backup_dest);

    clone_repo(url, branch, &temp_dest).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&temp_dest);
    })?;

    let had_existing = dest.exists();
    if had_existing {
        std::fs::rename(dest, &backup_dest)
            .map_err(|e| format!("failed to move existing cache aside: {e}"))?;
    }

    match std::fs::rename(&temp_dest, dest) {
        Ok(()) => {
            if had_existing {
                let _ = std::fs::remove_dir_all(&backup_dest);
            }
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&temp_dest);
            if had_existing && let Err(restore_err) = std::fs::rename(&backup_dest, dest) {
                return Err(format!(
                    "failed to install recloned cache: {e}; failed to restore original cache: {restore_err}; original cache preserved at {}",
                    backup_dest.display()
                ));
            }
            Err(format!("failed to install recloned cache: {e}"))
        }
    }
}

fn unique_reclone_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

/// Environment variables set on every git command to suppress interactive prompts.
pub const GIT_AUTH_SUPPRESSION_ENVS: [(&str, &str); 4] = [
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_ASKPASS", ""),
    ("GIT_LFS_SKIP_SMUDGE", "1"),
    ("GIT_SSH_COMMAND", "ssh -o BatchMode=yes"),
];

/// Git command with auth/LFS/SSH prompt suppression and `--no-optional-locks`.
pub fn git_command() -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    xai_tty_utils::detach_std_command(&mut cmd);
    cmd.stdin(std::process::Stdio::null());
    cmd.envs(xai_tty_utils::pager_env());
    for &(key, val) in &GIT_AUTH_SUPPRESSION_ENVS {
        cmd.env(key, val);
    }
    cmd.arg("--no-optional-locks");
    cmd
}

fn clone_cli_command(url: &str, branch: Option<&str>, dest: &Path) -> std::process::Command {
    let mut cmd = git_command();
    cmd.args(["clone", "--depth", "1"]);
    if let Some(b) = branch {
        cmd.args(["--branch", b]);
    }
    cmd.arg("--").arg(url).arg(dest.as_os_str());
    cmd
}

fn fetch_cli_command(repo_dir: &Path, branch: Option<&str>) -> std::process::Command {
    let mut cmd = git_command();
    cmd.current_dir(repo_dir).args([
        "fetch",
        "--depth",
        "1",
        "--",
        "origin",
        branch.unwrap_or("HEAD"),
    ]);
    cmd
}

/// Run a git command, wait up to `timeout`, kill+reap on hang. Errors on
/// timeout or non-zero exit; `what` names the operation in error messages.
fn run_git_timed(cmd: &mut Command, what: &str, timeout: Duration) -> Result<(), String> {
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to run git {what}: {e}"))?;
    match child.wait_timeout(timeout) {
        Ok(Some(status)) => {
            let mut stderr = Vec::new();
            if let Some(mut err) = child.stderr.take() {
                let _ = err.read_to_end(&mut stderr);
            }
            if status.success() {
                Ok(())
            } else {
                let stderr = String::from_utf8_lossy(&stderr);
                tracing::debug!("git {what} stderr: {stderr}");
                Err(git_failure_message(what, &stderr))
            }
        }
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(format!("git {what} timed out after {}s", timeout.as_secs()))
        }
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(format!("failed to wait for git {what}: {e}"))
        }
    }
}

/// Condense git stderr into a user-facing failure message. git writes
/// progress ("Cloning into ...") to stderr alongside real errors, so keep
/// only `fatal:`/`error:` lines, and translate the prompts-disabled auth
/// failure (we set GIT_TERMINAL_PROMPT=0 / ssh BatchMode) out of git-speak.
fn git_failure_message(what: &str, stderr: &str) -> String {
    const AUTH_PATTERNS: [&str; 3] = [
        "could not read Username",
        "could not read Password",
        "Authentication failed",
    ];
    if AUTH_PATTERNS.iter().any(|p| stderr.contains(p)) {
        return format!(
            "git {what} failed: authentication required or not a git repository (check the URL)"
        );
    }
    let salient: Vec<&str> = stderr
        .lines()
        .filter(|line| line.starts_with("fatal:") || line.starts_with("error:"))
        .collect();
    if salient.is_empty() {
        format!("git {what} failed: {}", stderr.trim())
    } else {
        format!("git {what} failed: {}", salient.join("; "))
    }
}

fn fetch_reset_cached_repo(repo_dir: &Path, branch: Option<&str>) -> Result<(), String> {
    let branch = branch
        .map(xai_grok_agent::plugins::git_install::validate_git_ref)
        .transpose()?;
    run_git_timed(
        &mut fetch_cli_command(repo_dir, branch),
        "fetch",
        NETWORK_OP_TIMEOUT,
    )?;

    let mut checkout_cmd = git_command();
    checkout_cmd
        .current_dir(repo_dir)
        .args(["checkout", "--detach", "FETCH_HEAD"]);
    run_git_timed(&mut checkout_cmd, "checkout", NETWORK_OP_TIMEOUT)?;

    let mut reset_cmd = git_command();
    reset_cmd
        .current_dir(repo_dir)
        .args(["reset", "--hard", "FETCH_HEAD"]);
    run_git_timed(&mut reset_cmd, "reset", NETWORK_OP_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_hash_is_deterministic() {
        let url = "https://github.com/xai-org/xai-plugin-marketplace.git";
        let h1 = cache_hash(url);
        let h2 = cache_hash(url);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 16);
    }

    #[test]
    fn cache_hash_differs_for_different_urls() {
        let h1 = cache_hash("https://github.com/a/b.git");
        let h2 = cache_hash("https://github.com/c/d.git");
        assert_ne!(h1, h2);
    }

    #[test]
    fn default_cache_root_under_grok() {
        let root = default_cache_root();
        assert!(root.to_string_lossy().contains("marketplace-cache"));
    }

    #[test]
    fn cli_git_args_terminate_options_before_operands() {
        let clone_cmd = clone_cli_command("repo", Some("main"), Path::new("dest"));
        let clone_args: Vec<_> = clone_cmd
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        assert_eq!(
            clone_args,
            [
                "--no-optional-locks",
                "clone",
                "--depth",
                "1",
                "--branch",
                "main",
                "--",
                "repo",
                "dest",
            ]
        );

        let fetch_cmd = fetch_cli_command(Path::new("repo"), Some("main"));
        let fetch_args: Vec<_> = fetch_cmd
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        assert_eq!(
            fetch_args,
            [
                "--no-optional-locks",
                "fetch",
                "--depth",
                "1",
                "--",
                "origin",
                "main",
            ]
        );
    }

    #[test]
    fn invalid_cache_operands_fail_before_cache_root_creation() {
        for (url, branch) in [
            ("--upload-pack=cmd", Some("main")),
            ("https://example.com/repo.git", Some("--upload-pack=cmd")),
        ] {
            let parent = tempfile::tempdir().unwrap();
            let cache_root = parent.path().join("cache");
            assert!(sync_source_cache(url, branch, &cache_root).is_err());
            assert!(!cache_root.exists());
        }
    }

    #[test]
    fn sync_source_cache_uses_ttl_by_default() {
        if !git_available() {
            eprintln!("skipping git-dependent test: git binary not available");
            return;
        }
        let remote = tempfile::tempdir().unwrap();
        init_remote_repo(remote.path());
        let cache_root = tempfile::tempdir().unwrap();
        let url = remote.path().to_string_lossy();

        let cache_dir = sync_source_cache(&url, Some("main"), cache_root.path()).unwrap();
        let fetch_head = cache_dir.join(".git").join("FETCH_HEAD");
        std::fs::write(&fetch_head, "ttl-sentinel").unwrap();
        let second_cache_dir = sync_source_cache(&url, Some("main"), cache_root.path()).unwrap();
        assert_eq!(second_cache_dir, cache_dir);
        assert_eq!(
            std::fs::read_to_string(&fetch_head).unwrap(),
            "ttl-sentinel"
        );
    }

    #[test]
    fn force_sync_source_cache_ignores_fresh_fetch_head() {
        if !git_available() {
            eprintln!("skipping git-dependent test: git binary not available");
            return;
        }
        let remote = tempfile::tempdir().unwrap();
        init_remote_repo(remote.path());
        let cache_root = tempfile::tempdir().unwrap();
        let url = remote.path().to_string_lossy();

        let cache_dir = sync_source_cache(&url, Some("main"), cache_root.path()).unwrap();
        let first_head = current_head(&cache_dir);
        add_commit(remote.path(), "second.txt", "second");

        let forced_cache_dir =
            force_sync_source_cache(&url, Some("main"), cache_root.path()).unwrap();
        assert_eq!(forced_cache_dir, cache_dir);
        assert_ne!(current_head(&cache_dir), first_head);
    }

    #[test]
    fn git_failure_message_maps_auth_prompt_to_plain_language() {
        let stderr = "Cloning into '/tmp/x'...\nfatal: could not read Username for 'https://mcp.linear.app': terminal prompts disabled\n";
        assert_eq!(
            git_failure_message("clone", stderr),
            "git clone failed: authentication required or not a git repository (check the URL)"
        );
    }

    #[test]
    fn git_failure_message_keeps_only_fatal_and_error_lines() {
        let stderr =
            "Cloning into '/tmp/x'...\nfatal: repository 'https://example.com/x.git/' not found\n";
        assert_eq!(
            git_failure_message("clone", stderr),
            "git clone failed: fatal: repository 'https://example.com/x.git/' not found"
        );
    }

    #[test]
    fn git_failure_message_falls_back_to_raw_stderr() {
        assert_eq!(
            git_failure_message("fetch", "something unusual\n"),
            "git fetch failed: something unusual"
        );
    }

    #[test]
    fn run_git_timed_kills_hung_process() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let start = Instant::now();
        let err = run_git_timed(&mut cmd, "sleep", Duration::from_millis(200)).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn cache_lease_blocks_concurrent_reclone_during_scan() {
        let cache_root = tempfile::tempdir().unwrap();
        let url = "https://example.com/repo.git";
        let hash = cache_hash(url);
        std::fs::create_dir_all(cache_root.path()).unwrap();
        let lock_path = cache_root.path().join(format!("{hash}.lock"));
        let lease = SourceCacheLease {
            path: cache_root.path().join(&hash),
            lock_file: acquire_cache_lock(&lock_path, Duration::from_millis(1)).unwrap(),
        };

        let start = Instant::now();
        let err = acquire_cache_lock(&lock_path, Duration::from_millis(50)).unwrap_err();
        assert!(err.contains("cache lock timeout"));
        assert!(start.elapsed() >= Duration::from_millis(50));
        drop(lease);
        let _lock = acquire_cache_lock(&lock_path, Duration::from_millis(1)).unwrap();
    }

    #[test]
    fn force_sync_source_cache_preserves_cache_when_reclone_fails() {
        if !git_available() {
            eprintln!("skipping git-dependent test: git binary not available");
            return;
        }
        let remote = tempfile::tempdir().unwrap();
        init_remote_repo(remote.path());
        let cache_root = tempfile::tempdir().unwrap();
        let url = remote.path().to_string_lossy();

        let cache_dir = sync_source_cache(&url, Some("main"), cache_root.path()).unwrap();
        std::fs::remove_dir_all(cache_dir.join(".git").join("objects")).unwrap();
        std::fs::remove_dir_all(remote.path()).unwrap();

        let result = force_sync_source_cache(&url, Some("main"), cache_root.path());
        assert!(result.is_err());
        assert!(cache_dir.exists());
        assert_eq!(
            std::fs::read_to_string(cache_dir.join("file.txt")).unwrap(),
            "initial"
        );
    }

    #[test]
    fn force_sync_source_cache_reclones_corrupt_cache() {
        if !git_available() {
            eprintln!("skipping git-dependent test: git binary not available");
            return;
        }
        let remote = tempfile::tempdir().unwrap();
        init_remote_repo(remote.path());
        let cache_root = tempfile::tempdir().unwrap();
        let url = remote.path().to_string_lossy();

        let cache_dir = sync_source_cache(&url, Some("main"), cache_root.path()).unwrap();
        std::fs::remove_dir_all(cache_dir.join(".git").join("objects")).unwrap();

        let forced_cache_dir =
            force_sync_source_cache(&url, Some("main"), cache_root.path()).unwrap();
        assert_eq!(forced_cache_dir, cache_dir);
        assert!(cache_dir.join(".git").join("objects").exists());
        assert_eq!(
            std::fs::read_to_string(cache_dir.join("file.txt")).unwrap(),
            "initial"
        );
    }

    fn init_remote_repo(path: &Path) {
        run_git(path, &["init", "--initial-branch", "main"]);
        run_git(path, &["config", "user.email", "test@example.com"]);
        run_git(path, &["config", "user.name", "Test User"]);
        add_commit(path, "file.txt", "initial");
    }

    fn add_commit(repo: &Path, file: &str, contents: &str) {
        std::fs::write(repo.join(file), contents).unwrap();
        run_git(repo, &["add", file]);
        run_git(repo, &["commit", "-m", file]);
    }

    fn current_head(repo: &Path) -> String {
        let output = git_command()
            .current_dir(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn git_available() -> bool {
        let git_bin = std::env::var("GIT_BIN_PATH").unwrap_or_else(|_| "git".to_string());
        std::process::Command::new(git_bin)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let git_bin = std::env::var("GIT_BIN_PATH").unwrap_or_else(|_| "git".to_string());
        let output = std::process::Command::new(git_bin)
            .current_dir(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "")
            .env("GIT_LFS_SKIP_SMUDGE", "1")
            .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
