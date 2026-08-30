//! Startup cleanup of stale pooled worktrees.
//!
//! A pre-warmed worktree pool (background fill, acquire/claim/release,
//! orphan adoption) once lived here but was never wired into production and
//! has been deleted. This cleanup path remains so `~/.grok/worktree_pool/`
//! directories left behind by dead agent instances are still reclaimed.
//!
//! Layout: each pool instance owned
//! `~/.grok/worktree_pool/<instance_id>/<pool_id>/` with a `.pid` liveness
//! file in the instance directory. Cleanup only touches directories whose
//! owning process is dead.

use std::path::{Path, PathBuf};

use xai_tty_utils::git_command;

use crate::util::grok_home::grok_home;

const WORKTREE_POOL_LOG: &str = "xai_worktree_pool";

/// Guard ensuring the expensive directory walk + `git worktree remove`
/// loop runs at most once per process.
static CLEANUP_ONCE: std::sync::Once = std::sync::Once::new();

/// Guard ensuring stale pool registration removal runs at most once per process.
static REGISTRATION_CLEANUP_ONCE: std::sync::Once = std::sync::Once::new();

/// Remove pooled worktrees belonging to dead agent instances.
///
/// **The expensive part (directory walk + `git worktree remove`) runs at
/// most once per process.** Multiple call sites (`initialize`,
/// `new_session`) may race to invoke this; only the first caller does the
/// real work, the rest return instantly.
///
/// Stale registration removal is gated separately so the first caller
/// that provides a `source_git_root` triggers it, even if the directory
/// cleanup already ran from an earlier call with `None`.
///
/// Multi-instance safe: iterates instance subdirectories under
/// `~/.grok/worktree_pool/`, reads each `.pid` file, and checks
/// whether the PID is still alive. Only cleans directories where the
/// owning process is dead.
///
/// Three-step cleanup:
/// 1. `git worktree remove --force` each worktree in a dead instance dir.
/// 2. Delete the instance directory from disk.
/// 3. Remove any remaining stale *pool-owned* registrations from the source
///    repo's `.git/worktrees/` as a safety net.
///
/// This is a **synchronous** function intended to be called via
/// `tokio::task::spawn_blocking` so it runs on the thread pool and
/// never competes with the agent's single-threaded `LocalSet`.
#[tracing::instrument(skip_all)]
pub fn cleanup_stale_pool_worktrees(source_git_root: Option<&Path>) {
    CLEANUP_ONCE.call_once(|| {
        cleanup_stale_pool_worktrees_inner();
    });

    if let Some(git_root) = source_git_root {
        let root = git_root.to_path_buf();
        REGISTRATION_CLEANUP_ONCE.call_once(move || {
            let removed = xai_fast_worktree::remove_stale_worktree_registrations_under(
                &root,
                &pool_base_directory(),
            );
            tracing::info!(
                target: WORKTREE_POOL_LOG,
                git_root = %root.display(),
                removed,
                "CLEANUP_PRUNE_DONE: stale pool registration removal completed"
            );
        });
    }
}

fn cleanup_stale_pool_worktrees_inner() {
    let pool_dir = pool_base_directory();
    tracing::info!(
        target: WORKTREE_POOL_LOG,
        pool_dir = %pool_dir.display(),
        "CLEANUP_START: scanning for dead instance pool directories"
    );
    let Ok(instances) = std::fs::read_dir(&pool_dir) else {
        tracing::info!(
            target: WORKTREE_POOL_LOG,
            pool_dir = %pool_dir.display(),
            "CLEANUP_SKIP: pool directory does not exist or unreadable"
        );
        return;
    };

    let mut cleaned_count = 0u32;
    let mut dead_instance_count = 0u32;

    for instance_entry in instances.flatten() {
        let instance_path = instance_entry.path();
        if !instance_path.is_dir() {
            continue;
        }

        let pid_alive = match std::fs::read_to_string(instance_path.join(".pid")) {
            Ok(contents) => match contents.trim().parse::<u32>() {
                Ok(pid) => {
                    let alive = crate::util::is_process_alive(pid);
                    tracing::info!(
                        target: WORKTREE_POOL_LOG,
                        instance_dir = %instance_path.display(),
                        pid,
                        alive,
                        "CLEANUP_CHECK_PID: checked instance PID"
                    );
                    alive
                }
                Err(_) => false,
            },
            Err(_) => false,
        };

        if pid_alive {
            tracing::debug!(
                instance_dir = %instance_path.display(),
                "Skipping live instance pool directory"
            );
            continue;
        }

        dead_instance_count += 1;

        tracing::info!(
            target: WORKTREE_POOL_LOG,
            instance_dir = %instance_path.display(),
            "CLEANUP_DEAD: found dead instance pool directory"
        );

        // Deregister and delete every worktree subdirectory. (The old pool
        // adopted structurally valid worktrees here; with the pool gone they
        // are reclaimed like any other stale directory.)
        if let Ok(entries) = std::fs::read_dir(&instance_path) {
            for wt_entry in entries.flatten() {
                let wt_path = wt_entry.path();
                if !wt_path.is_dir() {
                    continue; // skip .pid, marker files
                }

                let p = wt_path.to_string_lossy().to_string();
                tracing::info!(
                    target: WORKTREE_POOL_LOG,
                    path = %wt_path.display(),
                    "CLEANUP_GIT_REMOVE: running git worktree remove --force"
                );
                let result = git_command()
                    .args(["worktree", "remove", "--force", &p])
                    .output();
                tracing::info!(
                    target: WORKTREE_POOL_LOG,
                    path = %wt_path.display(),
                    success = result.as_ref().map(|o| o.status.success()).unwrap_or(false),
                    "CLEANUP_GIT_REMOVE_DONE: git worktree remove completed"
                );
                let _ = std::fs::remove_dir_all(&wt_path);
            }
        }

        let _ = std::fs::remove_dir_all(&instance_path);
        cleaned_count += 1;
    }

    tracing::info!(
        target: WORKTREE_POOL_LOG,
        dead_instance_count,
        cleaned_count,
        "CLEANUP_DONE: finished scanning for dead instances"
    );
}

/// The base pool directory under `~/.grok/`.
/// Instance-scoped directories live under this: `worktree_pool/<instance_id>/`.
fn pool_base_directory() -> PathBuf {
    grok_home().join("worktree_pool")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pool_base_directory() {
        let dir = pool_base_directory();
        assert!(dir.to_string_lossy().contains("worktree_pool"));
    }

    #[test]
    fn test_cleanup_stale_only_removes_dead_instances() {
        let live_dir =
            pool_base_directory().join(format!("live-instance-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&live_dir).unwrap();
        std::fs::write(live_dir.join(".pid"), std::process::id().to_string()).unwrap();

        let dead_dir =
            pool_base_directory().join(format!("dead-instance-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dead_dir).unwrap();
        std::fs::write(dead_dir.join(".pid"), "4000000000").unwrap();
        let fake_wt_a = dead_dir.join("fake-wt-aaa");
        let fake_wt_b = dead_dir.join("fake-wt-bbb");
        std::fs::create_dir_all(&fake_wt_a).unwrap();
        std::fs::create_dir_all(&fake_wt_b).unwrap();
        std::fs::write(fake_wt_a.join("file.txt"), "leftover").unwrap();
        std::fs::write(fake_wt_b.join("file.txt"), "leftover").unwrap();

        // Call the inner function directly to bypass the process-global
        // `Once` guard (other tests may have already triggered it).
        cleanup_stale_pool_worktrees_inner();

        assert!(
            live_dir.exists(),
            "Live pool's instance dir should NOT be cleaned"
        );
        assert!(!dead_dir.exists(), "Dead instance dir should be cleaned up");

        let _ = std::fs::remove_dir_all(&live_dir);
    }
}
