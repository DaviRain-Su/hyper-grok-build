//! Filesystem scanner for discovering worktrees not yet tracked in the DB.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::db::{
    WorktreeKind, WorktreeRecord, WorktreeStatus, id_from_path, now_epoch_secs, repo_name_from_path,
};

#[derive(Debug)]
pub struct DiscoveredWorktree {
    pub path: PathBuf,
    pub kind: WorktreeKind,
    pub creation_mode: &'static str,
    pub source_repo: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct DiscoveryReport {
    pub found: Vec<DiscoveredWorktree>,
    pub skipped: u64,
}

fn should_skip_entry(name: &str) -> bool {
    name.starts_with('.')
        || name.ends_with(".ready")
        || name.ends_with(".claimed")
        || name.ends_with(".claiming")
}

fn detect_creation_mode(worktree_path: &Path) -> &'static str {
    let git_entry = worktree_path.join(".git");
    if git_entry.is_file() {
        "linked"
    } else if git_entry.is_dir() {
        "standalone"
    } else {
        "unknown"
    }
}

fn detect_source_repo(worktree_path: &Path) -> Option<PathBuf> {
    let git_entry = worktree_path.join(".git");
    if git_entry.is_file() {
        let content = std::fs::read_to_string(&git_entry).ok()?;
        let gitdir = content.trim().strip_prefix("gitdir: ")?;
        // Walk up from .git/worktrees/<name> → .git → repo root
        Path::new(gitdir)
            .parent()?
            .parent()?
            .parent()
            .map(|p| p.to_path_buf())
    } else if git_entry.is_dir() {
        Some(worktree_path.to_path_buf())
    } else {
        None
    }
}

fn scan_two_level_dir(base_dir: &Path, kind: WorktreeKind, report: &mut DiscoveryReport) {
    let Ok(outer_entries) = std::fs::read_dir(base_dir) else {
        return;
    };

    for outer in outer_entries.flatten() {
        let outer_path = outer.path();
        if !outer_path.is_dir() {
            continue;
        }
        let outer_name = outer.file_name();
        if should_skip_entry(&outer_name.to_string_lossy()) {
            report.skipped += 1;
            continue;
        }

        let Ok(inner_entries) = std::fs::read_dir(&outer_path) else {
            continue;
        };
        for inner in inner_entries.flatten() {
            let path = inner.path();
            if !path.is_dir() || should_skip_entry(&inner.file_name().to_string_lossy()) {
                report.skipped += 1;
                continue;
            }
            report.found.push(DiscoveredWorktree {
                creation_mode: detect_creation_mode(&path),
                source_repo: detect_source_repo(&path),
                path,
                kind,
            });
        }
    }
}

pub fn discover_worktrees(grok_home: &Path) -> DiscoveryReport {
    let mut report = DiscoveryReport::default();
    scan_two_level_dir(
        &grok_home.join("worktrees"),
        WorktreeKind::Session,
        &mut report,
    );
    scan_two_level_dir(
        &grok_home.join("worktree_pool"),
        WorktreeKind::Pool,
        &mut report,
    );
    report
}

fn fs_creation_time(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.created())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or_else(now_epoch_secs)
}

impl DiscoveredWorktree {
    pub fn into_record(self) -> WorktreeRecord {
        let repo_name = self
            .source_repo
            .as_deref()
            .map(repo_name_from_path)
            .unwrap_or_else(|| "unknown".to_string());
        let source_repo = self.source_repo.unwrap_or_else(|| PathBuf::from("unknown"));
        let created_at = fs_creation_time(&self.path);
        // Match `WorktreeDb::get`, which looks up by canonical path.
        let path = dunce::canonicalize(&self.path).unwrap_or(self.path);

        WorktreeRecord {
            id: id_from_path(&path),
            path,
            source_repo,
            repo_name,
            kind: self.kind,
            creation_mode: self.creation_mode.to_owned(),
            git_ref: None,
            head_commit: None,
            session_id: None,
            creator_pid: None,
            created_at,
            last_accessed_at: None,
            status: WorktreeStatus::Alive,
            metadata: None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RebuildReport {
    pub discovered: u64,
    pub registered: u64,
    pub already_tracked: u64,
}

fn managed_worktree_roots(grok_home: &Path) -> [PathBuf; 2] {
    [grok_home.join("worktrees"), grok_home.join("worktree_pool")]
        .map(|root| dunce::canonicalize(&root).unwrap_or(root))
}

/// True when `path` is under a managed root (`worktrees/` or `worktree_pool/`).
/// Prefer already-canonical `path`; roots are canonicalized inside.
pub fn path_under_managed_worktree_roots(path: &Path, grok_home: &Path) -> bool {
    path_under_roots(path, &managed_worktree_roots(grok_home))
}

fn path_under_roots(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

pub fn rebuild_worktree_db(
    db: &crate::db::WorktreeDb,
    grok_home: &Path,
) -> anyhow::Result<RebuildReport> {
    let discovery = discover_worktrees(grok_home);
    let mut report = RebuildReport {
        discovered: discovery.found.len() as u64,
        ..Default::default()
    };
    let now = now_epoch_secs();
    let roots = managed_worktree_roots(grok_home);

    for wt in discovery.found {
        let path = dunce::canonicalize(&wt.path).unwrap_or_else(|_| wt.path.clone());
        // Refuse symlink escape outside managed roots.
        if !path_under_roots(&path, &roots) {
            tracing::warn!(
                path = %path.display(),
                "rebuild skipped path outside grok worktrees/worktree_pool"
            );
            continue;
        }
        let id = id_from_path(&path);
        let path_str = path.to_string_lossy();
        if db.get_by_id(&id)?.is_some() || db.get(&path_str)?.is_some() {
            report.already_tracked += 1;
            continue;
        }
        let mut rec = wt.into_record();
        // Touch so same-pass age GC does not reclaim solely from old FS mtime.
        rec.last_accessed_at = Some(now);
        db.register(&rec)?;
        report.registered += 1;
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_fake_linked_worktree(path: &Path, gitdir_target: &str) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join(".git"), format!("gitdir: {gitdir_target}\n")).unwrap();
    }

    fn make_fake_standalone_worktree(path: &Path) {
        std::fs::create_dir_all(path.join(".git")).unwrap();
    }

    #[test]
    fn discover_session_worktrees() {
        let tmp = tempfile::TempDir::new().unwrap();
        let grok_home = tmp.path();

        let wt = grok_home.join("worktrees/myrepo/worktree-abc123");
        make_fake_linked_worktree(&wt, "/repo/.git/worktrees/abc123");

        let report = discover_worktrees(grok_home);
        assert_eq!(report.found.len(), 1);
        assert_eq!(report.found[0].kind, WorktreeKind::Session);
        assert_eq!(report.found[0].creation_mode, "linked");
        assert_eq!(report.found[0].path, wt);
    }

    #[test]
    fn discover_pool_worktrees() {
        let tmp = tempfile::TempDir::new().unwrap();
        let grok_home = tmp.path();

        let wt = grok_home.join("worktree_pool/inst-1/pool-a");
        make_fake_standalone_worktree(&wt);

        let report = discover_worktrees(grok_home);
        assert_eq!(report.found.len(), 1);
        assert_eq!(report.found[0].kind, WorktreeKind::Pool);
        assert_eq!(report.found[0].creation_mode, "standalone");
    }

    #[test]
    fn skips_dot_prefixed_and_markers() {
        let tmp = tempfile::TempDir::new().unwrap();
        let grok_home = tmp.path();

        let base = grok_home.join("worktrees/myrepo");
        std::fs::create_dir_all(&base).unwrap();

        std::fs::create_dir_all(base.join(".tmp_creating")).unwrap();
        std::fs::create_dir_all(base.join(".hidden")).unwrap();
        std::fs::write(base.join("abc.ready"), "").unwrap();
        std::fs::write(base.join("abc.claimed"), "").unwrap();

        make_fake_standalone_worktree(&base.join("real-session"));

        let report = discover_worktrees(grok_home);
        assert_eq!(report.found.len(), 1);
        assert_eq!(report.found[0].path, base.join("real-session"));
        assert!(report.skipped > 0);
    }

    #[test]
    fn discover_empty_dirs_is_fine() {
        let tmp = tempfile::TempDir::new().unwrap();
        let report = discover_worktrees(tmp.path());
        assert!(report.found.is_empty());
        assert_eq!(report.skipped, 0);
    }

    #[test]
    fn rebuild_registers_and_skips_duplicates() {
        let tmp = tempfile::TempDir::new().unwrap();
        let grok_home = tmp.path();

        let wt = grok_home.join("worktrees/repo/worktree-sess1");
        make_fake_standalone_worktree(&wt);

        let db = crate::db::WorktreeDb::open_in_memory().unwrap();

        let r1 = rebuild_worktree_db(&db, grok_home).unwrap();
        assert_eq!(r1.discovered, 1);
        assert_eq!(r1.registered, 1);
        assert_eq!(r1.already_tracked, 0);

        let r2 = rebuild_worktree_db(&db, grok_home).unwrap();
        assert_eq!(r2.discovered, 1);
        assert_eq!(r2.registered, 0);
        assert_eq!(r2.already_tracked, 1);
    }

    #[test]
    fn rebuild_keeps_same_basename_worktrees_in_different_repos() {
        // The cross-repo eviction bug: two repos each have a `wt-abc`
        // worktree. Discovery + rebuild must register BOTH (distinct ids), not
        // collapse them into one and then permanently skip the other.
        let tmp = tempfile::TempDir::new().unwrap();
        let grok_home = tmp.path();

        let wt_a = grok_home.join("worktrees/repo-a/wt-abc");
        let wt_b = grok_home.join("worktrees/repo-b/wt-abc");
        make_fake_standalone_worktree(&wt_a);
        make_fake_standalone_worktree(&wt_b);

        let db = crate::db::WorktreeDb::open_in_memory().unwrap();
        let report = rebuild_worktree_db(&db, grok_home).unwrap();
        assert_eq!(report.discovered, 2);
        assert_eq!(
            report.registered, 2,
            "both same-basename worktrees must register"
        );

        let all = db.list(&crate::db::ListFilter::default()).unwrap();
        assert_eq!(all.len(), 2);
        assert!(db.get(&wt_a.to_string_lossy()).unwrap().is_some());
        assert!(db.get(&wt_b.to_string_lossy()).unwrap().is_some());

        // Idempotent: a second rebuild finds both already tracked, skips neither.
        let report2 = rebuild_worktree_db(&db, grok_home).unwrap();
        assert_eq!(report2.registered, 0);
        assert_eq!(report2.already_tracked, 2);
    }

    #[test]
    fn detect_source_repo_from_linked() {
        let tmp = tempfile::TempDir::new().unwrap();
        let wt = tmp.path().join("wt");
        let gitdir = "/home/user/myrepo/.git/worktrees/wt";
        make_fake_linked_worktree(&wt, gitdir);

        let source = detect_source_repo(&wt);
        assert_eq!(source, Some(PathBuf::from("/home/user/myrepo")));
    }

    #[test]
    fn rebuild_report_serde_round_trip() {
        let report = RebuildReport {
            discovered: 5,
            registered: 3,
            already_tracked: 2,
        };
        let json = serde_json::to_string(&report).unwrap();
        let deser: RebuildReport = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.discovered, 5);
        assert_eq!(deser.registered, 3);
        assert_eq!(deser.already_tracked, 2);
    }

    #[test]
    fn rebuild_sets_last_accessed_at() {
        let tmp = tempfile::TempDir::new().unwrap();
        let grok_home = tmp.path();
        let wt = grok_home.join("worktrees/repo/sess");
        make_fake_standalone_worktree(&wt);
        let db = crate::db::WorktreeDb::open_in_memory().unwrap();
        rebuild_worktree_db(&db, grok_home).unwrap();
        let rec = db.get(&wt.to_string_lossy()).unwrap().expect("registered");
        assert!(
            rec.last_accessed_at.is_some(),
            "rebuild must touch last_accessed_at for same-pass age safety"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rebuild_skips_symlink_escape_outside_managed_roots() {
        let tmp = tempfile::TempDir::new().unwrap();
        let grok_home = tmp.path().join("grok");
        let outside = tmp.path().join("outside-real");
        make_fake_standalone_worktree(&outside);
        let link_parent = grok_home.join("worktrees/repo");
        std::fs::create_dir_all(&link_parent).unwrap();
        std::os::unix::fs::symlink(&outside, link_parent.join("escaped")).unwrap();

        let db = crate::db::WorktreeDb::open_in_memory().unwrap();
        let report = rebuild_worktree_db(&db, &grok_home).unwrap();
        assert_eq!(report.discovered, 1);
        assert_eq!(report.registered, 0, "symlink escape must not register");
        assert!(
            db.list(&crate::db::ListFilter::default())
                .unwrap()
                .is_empty()
        );
        assert!(!path_under_managed_worktree_roots(
            &dunce::canonicalize(&outside).unwrap(),
            &grok_home
        ));
    }
}
