//! Throttled automatic worktree GC (feature `metadata`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::CleanupReport;
use crate::api::gc::{GcOptions, GcReport, age_path_enabled, gc_worktrees};
use crate::db::{ListFilter, WorktreeDb, WorktreeKind, now_epoch_secs, resolve_grok_home};
use crate::discovery::{RebuildReport, rebuild_worktree_db};
use crate::git::checkout::git_command;

pub const META_LAST_AUTO_GC_AT: &str = "last_auto_gc_at";
/// Independent throttle stamp for optional DB rebuild (not shared with GC).
pub const META_LAST_AUTO_REBUILD_AT: &str = "last_auto_rebuild_at";

/// `0` / `false` / `off` / empty disables auto-GC.
pub const ENV_AUTO_GC: &str = "GROK_WORKTREE_AUTO_GC";
/// `1` / `true` / `on` forces age-count without delete.
pub const ENV_AUTO_GC_DRY_RUN: &str = "GROK_WORKTREE_AUTO_GC_DRY_RUN";
/// Default max age in seconds (overrides TOML/remote when set and parseable).
pub const ENV_AUTO_GC_MAX_AGE: &str = "GROK_WORKTREE_AUTO_GC_MAX_AGE";
/// `1` / `true` / `on` enables optional discovery rebuild + stale git prune.
pub const ENV_AUTO_GC_REBUILD: &str = "GROK_WORKTREE_AUTO_GC_REBUILD";

pub const DEFAULT_MAX_AGE_SECS: i64 = 7 * 86400;
pub const DEFAULT_MIN_INTERVAL_SECS: i64 = 6 * 3600;
/// Rebuild is costlier than GC; default 24h until cost is measured in dogfood.
pub const DEFAULT_REBUILD_MIN_INTERVAL_SECS: i64 = 24 * 3600;

pub const MAX_AGE_SECS_MIN: i64 = 3600;
pub const MAX_AGE_SECS_MAX: i64 = 90 * 86400;
pub const MIN_INTERVAL_SECS_MIN: i64 = 60;
pub const MIN_INTERVAL_SECS_MAX: i64 = 7 * 86400;

/// Product default: Manual never age-expires unless config overrides.
pub fn default_max_age_by_kind() -> BTreeMap<WorktreeKind, Option<i64>> {
    BTreeMap::from([(WorktreeKind::Manual, None)])
}

/// Compile-time CWD-scan platforms (Linux/macOS). Runtime failure fail-closes in `gc_worktrees`.
pub fn process_cwd_scan_available() -> bool {
    cfg!(any(target_os = "linux", target_os = "macos"))
}

/// Real age-expiry when a CWD-scan platform, or dry-run metrics without deletes.
pub fn age_expiry_allowed(scan_platform: bool, dry_run: bool) -> bool {
    scan_platform || dry_run
}

/// One local/remote config layer (`max_age_by_kind`: `Some(secs)` or `None`=never).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorktreeAutoGcLayer {
    pub enabled: Option<bool>,
    pub max_age_secs: Option<u64>,
    pub min_interval_secs: Option<u64>,
    pub dry_run: Option<bool>,
    pub include_orphan_snapshots: Option<bool>,
    pub max_age_by_kind: BTreeMap<WorktreeKind, Option<u64>>,
    /// Optional discovery rebuild + stale `.git/worktrees/` prune (default off).
    pub include_rebuild: Option<bool>,
    /// Independent rebuild throttle; absent ⇒ 24h.
    pub rebuild_min_interval_secs: Option<u64>,
}

/// Policy after env / TOML / remote merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedWorktreeAutoGc {
    pub enabled: bool,
    pub max_age_secs: i64,
    pub min_interval_secs: i64,
    pub dry_run: bool,
    pub include_orphan_snapshots: bool,
    pub max_age_by_kind: BTreeMap<WorktreeKind, Option<i64>>,
    /// Off by default until rebuild cost is measured.
    pub include_rebuild: bool,
    pub rebuild_min_interval_secs: i64,
}

impl Default for ResolvedWorktreeAutoGc {
    fn default() -> Self {
        Self {
            enabled: true,
            max_age_secs: DEFAULT_MAX_AGE_SECS,
            min_interval_secs: DEFAULT_MIN_INTERVAL_SECS,
            dry_run: false,
            include_orphan_snapshots: cfg!(target_os = "linux"),
            max_age_by_kind: default_max_age_by_kind(),
            include_rebuild: false,
            rebuild_min_interval_secs: DEFAULT_REBUILD_MIN_INTERVAL_SECS,
        }
    }
}

impl ResolvedWorktreeAutoGc {
    /// Defaults plus env kill / dry-run / max-age only.
    pub fn from_env_only() -> Self {
        resolve_worktree_auto_gc_from_layers(None, None)
    }
}

fn clamp_kind_age(secs: Option<u64>) -> Option<i64> {
    secs.map(clamp_max_age_secs)
}

/// Merge kind maps: product default Manual=never, then remote, then local.
fn merge_max_age_by_kind(
    local: Option<&BTreeMap<WorktreeKind, Option<u64>>>,
    remote: Option<&BTreeMap<WorktreeKind, Option<u64>>>,
) -> BTreeMap<WorktreeKind, Option<i64>> {
    let mut map = default_max_age_by_kind();
    if let Some(r) = remote {
        for (&k, &v) in r {
            map.insert(k, clamp_kind_age(v));
        }
    }
    if let Some(l) = local {
        for (&k, &v) in l {
            map.insert(k, clamp_kind_age(v));
        }
    }
    map
}

/// Precedence: env > local > remote > defaults (with numeric clamps).
pub fn resolve_worktree_auto_gc_from_layers(
    local: Option<&WorktreeAutoGcLayer>,
    remote: Option<&WorktreeAutoGcLayer>,
) -> ResolvedWorktreeAutoGc {
    let enabled = if env_auto_gc_disabled() {
        false
    } else {
        local
            .and_then(|s| s.enabled)
            .or(remote.and_then(|s| s.enabled))
            .unwrap_or(true)
    };

    let max_age_secs = env_auto_gc_max_age()
        .or(local.and_then(|s| s.max_age_secs))
        .or(remote.and_then(|s| s.max_age_secs))
        .map(clamp_max_age_secs)
        .unwrap_or(DEFAULT_MAX_AGE_SECS);

    let min_interval_secs = local
        .and_then(|s| s.min_interval_secs)
        .or(remote.and_then(|s| s.min_interval_secs))
        .map(clamp_min_interval_secs)
        .unwrap_or(DEFAULT_MIN_INTERVAL_SECS);

    let dry_run = if env_auto_gc_dry_run() {
        true
    } else {
        local
            .and_then(|s| s.dry_run)
            .or(remote.and_then(|s| s.dry_run))
            .unwrap_or(false)
    };

    let include_orphan_snapshots = local
        .and_then(|s| s.include_orphan_snapshots)
        .or(remote.and_then(|s| s.include_orphan_snapshots))
        .unwrap_or(cfg!(target_os = "linux"));

    // Env REBUILD=1 forces on; config cannot disable over env.
    let include_rebuild = if env_auto_gc_rebuild() {
        true
    } else {
        local
            .and_then(|s| s.include_rebuild)
            .or(remote.and_then(|s| s.include_rebuild))
            .unwrap_or(false)
    };

    let rebuild_min_interval_secs = local
        .and_then(|s| s.rebuild_min_interval_secs)
        .or(remote.and_then(|s| s.rebuild_min_interval_secs))
        .map(clamp_min_interval_secs)
        .unwrap_or(DEFAULT_REBUILD_MIN_INTERVAL_SECS);

    ResolvedWorktreeAutoGc {
        enabled,
        max_age_secs,
        min_interval_secs,
        dry_run,
        include_orphan_snapshots,
        max_age_by_kind: merge_max_age_by_kind(
            local.map(|s| &s.max_age_by_kind),
            remote.map(|s| &s.max_age_by_kind),
        ),
        include_rebuild,
        rebuild_min_interval_secs,
    }
}

/// Runtime options for [`maybe_auto_gc`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoGcOptions {
    /// Env kill still wins inside [`maybe_auto_gc`].
    pub enabled: bool,
    pub max_age_secs: i64,
    pub min_interval_secs: i64,
    pub include_orphan_snapshots: bool,
    /// Env dry-run is re-applied inside [`maybe_auto_gc`].
    pub dry_run: bool,
    pub max_age_by_kind: BTreeMap<WorktreeKind, Option<i64>>,
    /// When true, optionally rebuild DB from disk + prune stale git registrations.
    pub include_rebuild: bool,
    pub rebuild_min_interval_secs: i64,
}

impl Default for AutoGcOptions {
    fn default() -> Self {
        Self::from_resolved(ResolvedWorktreeAutoGc::default())
    }
}

impl AutoGcOptions {
    pub fn from_resolved(policy: ResolvedWorktreeAutoGc) -> Self {
        Self {
            enabled: policy.enabled,
            max_age_secs: policy.max_age_secs,
            min_interval_secs: policy.min_interval_secs,
            include_orphan_snapshots: policy.include_orphan_snapshots,
            dry_run: policy.dry_run,
            max_age_by_kind: policy.max_age_by_kind,
            include_rebuild: policy.include_rebuild,
            rebuild_min_interval_secs: policy.rebuild_min_interval_secs,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutoGcOutcome {
    Disabled,
    Throttled,
    Ran,
}

#[derive(Debug)]
pub struct AutoGcReport {
    pub outcome: AutoGcOutcome,
    pub gc: Option<GcReport>,
    pub overlay: Option<CleanupReport>,
    pub btrfs: Option<CleanupReport>,
    pub age_expiry_enabled: bool,
    pub stamped: bool,
    /// Present only when a rebuild pass ran in this invocation.
    pub rebuild: Option<RebuildReport>,
    /// True when `last_auto_rebuild_at` was written this pass.
    pub rebuild_stamped: bool,
    /// Entries removed from known source repos' `.git/worktrees/` via prune.
    pub stale_registrations_cleaned: u64,
}

impl AutoGcReport {
    fn empty(outcome: AutoGcOutcome) -> Self {
        Self {
            outcome,
            gc: None,
            overlay: None,
            btrfs: None,
            age_expiry_enabled: false,
            stamped: false,
            rebuild: None,
            rebuild_stamped: false,
            stale_registrations_cleaned: 0,
        }
    }

    fn disabled() -> Self {
        Self::empty(AutoGcOutcome::Disabled)
    }

    fn throttled() -> Self {
        Self::empty(AutoGcOutcome::Throttled)
    }
}

fn env_var_truthy(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on" | "enabled"
        ),
        Err(_) => false,
    }
}

fn env_var_disabled(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off" | "disabled" | ""
        ),
        Err(_) => false,
    }
}

pub fn env_auto_gc_disabled() -> bool {
    env_var_disabled(ENV_AUTO_GC)
}

pub fn env_auto_gc_dry_run() -> bool {
    env_var_truthy(ENV_AUTO_GC_DRY_RUN)
}

pub fn env_auto_gc_rebuild() -> bool {
    env_var_truthy(ENV_AUTO_GC_REBUILD)
}

/// Parse `GROK_WORKTREE_AUTO_GC_MAX_AGE` as seconds; invalid/absent → None.
pub fn env_auto_gc_max_age() -> Option<u64> {
    match std::env::var(ENV_AUTO_GC_MAX_AGE) {
        Ok(v) => {
            let s = v.trim();
            if s.is_empty() {
                return None;
            }
            s.parse::<u64>().ok()
        }
        Err(_) => None,
    }
}

pub fn clamp_max_age_secs(v: u64) -> i64 {
    // Cap in u64 so values > i64::MAX do not wrap negative.
    let capped = v.min(MAX_AGE_SECS_MAX as u64) as i64;
    capped.max(MAX_AGE_SECS_MIN)
}

pub fn clamp_min_interval_secs(v: u64) -> i64 {
    let capped = v.min(MIN_INTERVAL_SECS_MAX as u64) as i64;
    capped.max(MIN_INTERVAL_SECS_MIN)
}

/// Auto-path `GcOptions` (`force=false`, kind policy map, protect paths canon once).
pub fn build_auto_gc_options(auto_opts: &AutoGcOptions, protect_paths: Vec<PathBuf>) -> GcOptions {
    build_auto_gc_options_with_dry_run(auto_opts, protect_paths, auto_opts.dry_run)
}

fn build_auto_gc_options_with_dry_run(
    auto_opts: &AutoGcOptions,
    protect_paths: Vec<PathBuf>,
    dry_run: bool,
) -> GcOptions {
    let age_allowed = age_expiry_allowed(process_cwd_scan_available(), dry_run);
    let protect_paths = protect_paths
        .into_iter()
        .map(|p| dunce::canonicalize(&p).unwrap_or(p))
        .collect();
    // Clone kind map only when the age path is live; platform-off drops it.
    let max_age_by_kind = if age_allowed {
        auto_opts.max_age_by_kind.clone()
    } else {
        BTreeMap::new()
    };
    GcOptions {
        max_age_secs: age_allowed.then_some(auto_opts.max_age_secs),
        force: false,
        dry_run,
        protect_paths,
        skip_kinds: vec![],
        max_age_by_kind,
    }
}

/// Future stamps (clock skew) are treated as due so throttle cannot black out forever.
pub(crate) fn is_throttled(now: i64, last: i64, min_interval_secs: i64) -> bool {
    if last > now {
        return false;
    }
    now.saturating_sub(last) < min_interval_secs.max(0)
}

/// Throttled auto-GC. `Ok` always carries a report; `Err` means infrastructure
/// failure before/during GC (not stamped). Env kill/dry-run/rebuild override
/// raw options. GC throttle short-circuits the whole pass (including rebuild).
pub fn maybe_auto_gc(db: &WorktreeDb, auto_opts: &AutoGcOptions) -> Result<AutoGcReport> {
    if env_auto_gc_disabled() || !auto_opts.enabled {
        tracing::debug!("auto worktree gc disabled");
        return Ok(AutoGcReport::disabled());
    }

    let dry_run = auto_opts.dry_run || env_auto_gc_dry_run();
    let include_rebuild = auto_opts.include_rebuild || env_auto_gc_rebuild();

    let now = now_epoch_secs();
    // GC meta: fail closed on read Err; unparseable fails open. Throttle skips rebuild too.
    if let Some(ts) = db.get_meta(META_LAST_AUTO_GC_AT)? {
        match ts.parse::<i64>() {
            Ok(last) if is_throttled(now, last, auto_opts.min_interval_secs) => {
                tracing::debug!(
                    last_auto_gc_at = last,
                    min_interval_secs = auto_opts.min_interval_secs,
                    "auto worktree gc throttled"
                );
                return Ok(AutoGcReport::throttled());
            }
            Ok(_) => {}
            Err(_) => {
                tracing::warn!(
                    value = %ts,
                    "auto worktree gc ignoring unparseable last_auto_gc_at; running reclaim"
                );
            }
        }
    }

    // Rebuild before the prune-repo snapshot so newly registered worktrees'
    // source repos are included. Snapshot still happens before dead-GC so
    // sole-dead source repos remain in the set after unregister.
    //
    // Rebuild meta is **not** stamped here: if GC fails after a successful
    // rebuild, we must leave rebuild unthrottled so the next pass can pick up
    // worktrees created between this rebuild and the failed GC.
    let (rebuild, rebuild_due_to_stamp) = maybe_run_rebuild(
        db,
        include_rebuild,
        dry_run,
        auto_opts.rebuild_min_interval_secs,
        now,
    );

    let prune_repos = if include_rebuild && !dry_run {
        collect_source_repos_for_prune(db)
    } else {
        BTreeSet::new()
    };

    let mut protect_paths = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        protect_paths.push(cwd);
    }

    let gc_opts = build_auto_gc_options_with_dry_run(auto_opts, protect_paths, dry_run);
    let age_expiry_enabled = age_path_enabled(&gc_opts);
    debug_assert!(!gc_opts.force);

    let gc_report = match gc_worktrees(db, &gc_opts) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                error = %e,
                rebuild_ran = rebuild.is_some(),
                "auto worktree gc failed; rebuild meta left unstamped so next pass can re-discover"
            );
            return Err(e);
        }
    };

    if gc_report.remove_failed > 0 {
        tracing::warn!(
            remove_failed = gc_report.remove_failed,
            "auto worktree gc had remove failures"
        );
    }

    let (overlay, btrfs) = run_orphan_cleaners(dry_run, auto_opts.include_orphan_snapshots);

    // Prune each full pass when opted in (cheap vs discovery; not rebuild-throttled).
    let stale_registrations_cleaned = if include_rebuild && !dry_run {
        prune_stale_git_worktree_registrations(&prune_repos)
    } else {
        0
    };

    let stamp_now = now_epoch_secs();
    // Stamp rebuild only after GC succeeds (see maybe_run_rebuild).
    let rebuild_stamped = if rebuild_due_to_stamp {
        match db.set_meta(META_LAST_AUTO_REBUILD_AT, &stamp_now.to_string()) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "auto worktree rebuild failed to stamp meta");
                false
            }
        }
    } else {
        false
    };
    let stamped = match db.set_meta(META_LAST_AUTO_GC_AT, &stamp_now.to_string()) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(error = %e, "auto worktree gc failed to stamp meta");
            false
        }
    };

    let overlay_errors = overlay.as_ref().map(|r| r.errors).unwrap_or(0);
    let btrfs_errors = btrfs.as_ref().map(|r| r.errors).unwrap_or(0);
    let rebuild_discovered = rebuild.as_ref().map(|r| r.discovered).unwrap_or(0);
    let rebuild_registered = rebuild.as_ref().map(|r| r.registered).unwrap_or(0);
    tracing::info!(
        age_expiry_enabled,
        dead_removed = gc_report.dead_removed,
        expired_removed = gc_report.expired_removed,
        skipped_alive = gc_report.skipped_alive,
        remove_failed = gc_report.remove_failed,
        overlay_errors,
        btrfs_errors,
        rebuild_discovered,
        rebuild_registered,
        stale_registrations_cleaned,
        dry_run,
        stamped,
        rebuild_stamped,
        "auto worktree gc complete"
    );

    Ok(AutoGcReport {
        outcome: AutoGcOutcome::Ran,
        gc: Some(gc_report),
        overlay,
        btrfs,
        age_expiry_enabled,
        stamped,
        rebuild,
        rebuild_stamped,
        stale_registrations_cleaned,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RebuildMetaClass {
    Due,
    Throttled,
    /// Meta read failed — skip rebuild, do not abort GC.
    SkipFailed,
}

/// `Err` ⇒ skip rebuild only (GC continues).
fn classify_rebuild_meta(
    meta: Result<Option<String>>,
    now: i64,
    rebuild_min_interval_secs: i64,
) -> RebuildMetaClass {
    match meta {
        Err(e) => {
            tracing::warn!(
                error = %e,
                "auto worktree rebuild skipped: meta read failed; continuing GC"
            );
            RebuildMetaClass::SkipFailed
        }
        Ok(None) => RebuildMetaClass::Due,
        Ok(Some(ts)) => match ts.parse::<i64>() {
            Ok(last) if is_throttled(now, last, rebuild_min_interval_secs) => {
                tracing::debug!(
                    last_auto_rebuild_at = last,
                    rebuild_min_interval_secs,
                    "auto worktree rebuild throttled"
                );
                RebuildMetaClass::Throttled
            }
            Ok(_) => RebuildMetaClass::Due,
            Err(_) => {
                tracing::warn!(
                    value = %ts,
                    "auto worktree rebuild ignoring unparseable last_auto_rebuild_at"
                );
                RebuildMetaClass::Due
            }
        },
    }
}

/// Optional rebuild; never fails the GC pass.
///
/// Returns `(report, due_to_stamp)`. Stamp is applied by the caller **only
/// after** GC succeeds — stamping here would throttle rebuild while GC can
/// still `Err` and leave `last_auto_gc_at` unstamped.
fn maybe_run_rebuild(
    db: &WorktreeDb,
    include_rebuild: bool,
    dry_run: bool,
    rebuild_min_interval_secs: i64,
    now: i64,
) -> (Option<RebuildReport>, bool) {
    if !include_rebuild || dry_run {
        return (None, false);
    }

    match classify_rebuild_meta(
        db.get_meta(META_LAST_AUTO_REBUILD_AT),
        now,
        rebuild_min_interval_secs,
    ) {
        RebuildMetaClass::Due => {}
        RebuildMetaClass::Throttled | RebuildMetaClass::SkipFailed => return (None, false),
    }

    let home = match resolve_grok_home() {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error = %e, "auto worktree rebuild skipped: grok home unresolved");
            return (None, false);
        }
    };

    match rebuild_worktree_db(db, &home) {
        Ok(report) => {
            tracing::info!(
                discovered = report.discovered,
                registered = report.registered,
                already_tracked = report.already_tracked,
                "auto worktree db rebuild complete"
            );
            // Defer META_LAST_AUTO_REBUILD_AT until after GC succeeds.
            (Some(report), true)
        }
        Err(e) => {
            tracing::warn!(error = %e, "auto worktree rebuild failed; continuing GC");
            (None, false)
        }
    }
}

/// Distinct non-unknown `source_repo` values (alive + dead) for prune.
fn collect_source_repos_for_prune(db: &WorktreeDb) -> BTreeSet<PathBuf> {
    let filter = ListFilter {
        include_dead: true,
        ..Default::default()
    };
    let Ok(records) = db.list(&filter) else {
        tracing::warn!("auto worktree prune skipped: list failed");
        return BTreeSet::new();
    };
    records
        .into_iter()
        .filter(|r| r.source_repo.as_os_str() != "unknown")
        .map(|r| r.source_repo)
        .collect()
}

fn prune_stale_git_worktree_registrations(repos: &BTreeSet<PathBuf>) -> u64 {
    let cleaned: u64 = repos
        .iter()
        .filter(|repo| repo.is_dir())
        .map(|repo| prune_stale_registrations_in_repo(repo))
        .fold(0u64, u64::saturating_add);
    if cleaned > 0 {
        tracing::info!(
            stale_registrations_cleaned = cleaned,
            "auto worktree stale git registrations pruned"
        );
    }
    cleaned
}

fn count_git_worktree_registrations(git_worktrees_dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(git_worktrees_dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .count() as u64
}

fn prune_stale_registrations_in_repo(source_repo: &Path) -> u64 {
    let git_worktrees = source_repo.join(".git").join("worktrees");
    let before = count_git_worktree_registrations(&git_worktrees);

    let output = git_command()
        .args(["worktree", "prune"])
        .current_dir(source_repo)
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let after = count_git_worktree_registrations(&git_worktrees);
            before.saturating_sub(after)
        }
        Ok(o) => {
            tracing::warn!(
                source_repo = %source_repo.display(),
                status = %o.status,
                stderr = %String::from_utf8_lossy(&o.stderr),
                "git worktree prune failed"
            );
            0
        }
        Err(e) => {
            tracing::warn!(
                source_repo = %source_repo.display(),
                error = %e,
                "git worktree prune failed to spawn"
            );
            0
        }
    }
}

fn run_orphan_cleaners(
    dry_run: bool,
    include_orphan_snapshots: bool,
) -> (Option<CleanupReport>, Option<CleanupReport>) {
    #[cfg(target_os = "linux")]
    {
        if dry_run || !include_orphan_snapshots {
            return (None, None);
        }
        let overlay = crate::cleanup_orphaned_overlay_snapshots();
        let btrfs = crate::cleanup_orphaned_btrfs_snapshots();
        if overlay.errors > 0 {
            tracing::warn!(
                errors = overlay.errors,
                "auto worktree gc overlay orphan cleanup had errors"
            );
        }
        if btrfs.errors > 0 {
            tracing::warn!(
                errors = btrfs.errors,
                "auto worktree gc btrfs orphan cleanup had errors"
            );
        }
        (Some(overlay), Some(btrfs))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (dry_run, include_orphan_snapshots);
        (None, None)
    }
}

/// Env + built-in defaults only (no TOML/remote).
pub fn maybe_auto_gc_default() -> Result<AutoGcReport> {
    let db = WorktreeDb::open_default()?;
    let opts = AutoGcOptions::from_resolved(ResolvedWorktreeAutoGc::from_env_only());
    maybe_auto_gc(&db, &opts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{WorktreeRecord, WorktreeStatus};
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn env_guard() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn clear_auto_gc_env() {
        unsafe {
            std::env::remove_var(ENV_AUTO_GC);
            std::env::remove_var(ENV_AUTO_GC_DRY_RUN);
            std::env::remove_var(ENV_AUTO_GC_MAX_AGE);
            std::env::remove_var(ENV_AUTO_GC_REBUILD);
        }
    }

    fn make_rec(id: &str, path: PathBuf, kind: WorktreeKind, created_at: i64) -> WorktreeRecord {
        WorktreeRecord {
            id: id.to_string(),
            path,
            source_repo: "/repo".into(),
            repo_name: "repo".to_string(),
            kind,
            creation_mode: "linked".to_string(),
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

    fn opts_enabled_no_orphans(dry_run: bool) -> AutoGcOptions {
        AutoGcOptions {
            min_interval_secs: 0,
            include_orphan_snapshots: false,
            dry_run,
            ..AutoGcOptions::default()
        }
    }

    /// Git repo + stale linked-worktree registration (working tree deleted).
    fn plant_stale_git_worktree(repo: &Path, wt: &Path) {
        std::fs::create_dir_all(repo).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init"])
                .current_dir(repo)
                .output()
                .unwrap()
                .status
                .success()
        );
        for (k, v) in [("user.email", "t@test"), ("user.name", "t")] {
            assert!(
                std::process::Command::new("git")
                    .args(["config", k, v])
                    .current_dir(repo)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
        std::fs::write(repo.join("f.txt"), b"x").unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["add", "f.txt"])
                .current_dir(repo)
                .output()
                .unwrap()
                .status
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args(["commit", "-m", "i"])
                .current_dir(repo)
                .output()
                .unwrap()
                .status
                .success()
        );
        let add_wt = std::process::Command::new("git")
            .args(["worktree", "add", "--detach", wt.to_str().unwrap(), "HEAD"])
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            add_wt.status.success(),
            "worktree add failed: {}",
            String::from_utf8_lossy(&add_wt.stderr)
        );
        std::fs::remove_dir_all(wt).unwrap();
    }

    fn count_regs(repo: &Path) -> usize {
        std::fs::read_dir(repo.join(".git/worktrees"))
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .filter(|e| e.path().is_dir())
                    .count()
            })
            .unwrap_or(0)
    }

    #[test]
    fn build_auto_gc_options_never_sets_force() {
        let opts = AutoGcOptions {
            max_age_secs: 1,
            min_interval_secs: 1,
            include_orphan_snapshots: false,
            ..AutoGcOptions::default()
        };
        let gc = build_auto_gc_options(&opts, Vec::new());
        assert!(!gc.force, "auto path must never set force=true");
        assert!(gc.skip_kinds.is_empty());
        assert_eq!(gc.max_age_by_kind.get(&WorktreeKind::Manual), Some(&None));
    }

    #[test]
    fn age_expiry_allowed_table() {
        assert!(!age_expiry_allowed(false, false));
        assert!(
            age_expiry_allowed(false, true),
            "dry_run enables age metrics"
        );
        assert!(
            age_expiry_allowed(true, false),
            "scan platform enables real age"
        );
        assert!(age_expiry_allowed(true, true));
    }

    #[test]
    fn process_cwd_scan_available_implies_usable_scan() {
        if !process_cwd_scan_available() {
            assert!(
                !age_expiry_allowed(false, false),
                "no scan platform ⇒ non-dry-run age off"
            );
            return;
        }
        // Serialize with chdir tests (process-global cwd / scan validation).
        let _cwd_lock = crate::api::cwd_test_guard();
        match crate::api::gc::live_process_cwds() {
            crate::api::gc::LiveCwdScan::Ok(cwds) => {
                let cwd = std::env::current_dir().unwrap();
                assert!(
                    cwds.iter().any(|c| {
                        c == &cwd
                            || dunce::canonicalize(c)
                                .ok()
                                .and_then(|cc| dunce::canonicalize(&cwd).ok().map(|w| cc == w))
                                .unwrap_or(false)
                    }),
                    "available scan must observe own CWD"
                );
            }
            other => panic!("process_cwd_scan_available but scan unusable: {other:?}"),
        }
    }

    #[test]
    fn build_auto_gc_options_age_expiry_requires_scan_or_dry_run() {
        let _g = env_guard();
        clear_auto_gc_env();
        let opts = AutoGcOptions {
            max_age_secs: 999,
            min_interval_secs: 1,
            include_orphan_snapshots: true,
            dry_run: false,
            ..AutoGcOptions::default()
        };
        let gc = build_auto_gc_options(&opts, Vec::new());
        assert_eq!(
            gc.max_age_secs.is_some(),
            age_expiry_allowed(process_cwd_scan_available(), false)
        );
        if process_cwd_scan_available() {
            assert_eq!(gc.max_age_secs, Some(999));
            assert_eq!(gc.max_age_by_kind.get(&WorktreeKind::Manual), Some(&None));
        } else {
            assert_eq!(gc.max_age_secs, None);
            assert!(gc.max_age_by_kind.is_empty());
        }
    }

    #[test]
    fn build_auto_gc_options_dry_run_may_set_max_age_without_cwd_scan() {
        let _g = env_guard();
        clear_auto_gc_env();
        let opts = AutoGcOptions {
            max_age_secs: 123,
            min_interval_secs: 1,
            include_orphan_snapshots: false,
            dry_run: true,
            ..AutoGcOptions::default()
        };
        let gc = build_auto_gc_options(&opts, Vec::new());
        assert_eq!(gc.max_age_secs, Some(123));
        assert!(gc.dry_run);
        assert!(!gc.force);
        assert_eq!(gc.max_age_by_kind.get(&WorktreeKind::Manual), Some(&None));
        assert!(age_expiry_allowed(process_cwd_scan_available(), true));
    }

    #[test]
    fn maybe_auto_gc_age_expiry_tracks_scan_for_real_and_dry_run() {
        // Complementary: dry_run always enables age metrics; real run only when scan works.
        let _g = env_guard();
        clear_auto_gc_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let db = WorktreeDb::open(tmp.path()).unwrap();

        let dry = maybe_auto_gc(&db, &opts_enabled_no_orphans(true)).unwrap();
        assert_eq!(dry.outcome, AutoGcOutcome::Ran);
        assert!(
            dry.age_expiry_enabled,
            "dry_run must enable age metrics even without CWD scan"
        );

        db.set_meta(META_LAST_AUTO_GC_AT, "0").unwrap(); // reset throttle
        let real = maybe_auto_gc(&db, &opts_enabled_no_orphans(false)).unwrap();
        assert_eq!(real.outcome, AutoGcOutcome::Ran);
        assert_eq!(
            real.age_expiry_enabled,
            age_expiry_allowed(process_cwd_scan_available(), false),
            "non-dry-run age_expiry must match age_expiry_allowed(scan, false)"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn maybe_auto_gc_real_expires_unguarded_protects_live_creator() {
        // Age path needs a successful CWD scan — serialize with chdir tests.
        let _g = env_guard();
        let _cwd_lock = crate::api::cwd_test_guard();
        clear_auto_gc_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let db = WorktreeDb::open(tmp.path()).unwrap();

        let doomed = tmp.path().join("doomed-session");
        std::fs::create_dir(&doomed).unwrap();
        db.register(&make_rec(
            "doomed",
            doomed.clone(),
            WorktreeKind::Session,
            1,
        ))
        .unwrap();

        let kept = tmp.path().join("kept-session");
        std::fs::create_dir(&kept).unwrap();
        let mut live = make_rec("kept", kept.clone(), WorktreeKind::Session, 1);
        live.creator_pid = Some(std::process::id());
        db.register(&live).unwrap();

        let report = maybe_auto_gc(
            &db,
            &AutoGcOptions {
                max_age_secs: 0,
                min_interval_secs: 0,
                include_orphan_snapshots: false,
                dry_run: false,
                ..AutoGcOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.outcome, AutoGcOutcome::Ran);
        assert!(report.age_expiry_enabled);
        assert!(
            !doomed.exists(),
            "unguarded expired session must be age-deleted on scan platforms"
        );
        assert!(
            kept.exists(),
            "live creator_pid must protect the other tree"
        );
        let gc = report.gc.as_ref().unwrap();
        assert!(gc.expired_removed >= 1);
        assert!(gc.skipped_alive >= 1);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn maybe_auto_gc_default_never_expires_manual() {
        let _g = env_guard();
        let _cwd_lock = crate::api::cwd_test_guard();
        clear_auto_gc_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let db = WorktreeDb::open(tmp.path()).unwrap();

        let session = tmp.path().join("sess");
        let manual = tmp.path().join("man");
        std::fs::create_dir(&session).unwrap();
        std::fs::create_dir(&manual).unwrap();
        db.register(&make_rec("s", session.clone(), WorktreeKind::Session, 1))
            .unwrap();
        db.register(&make_rec("m", manual.clone(), WorktreeKind::Manual, 1))
            .unwrap();

        let report = maybe_auto_gc(
            &db,
            &AutoGcOptions {
                max_age_secs: 0,
                min_interval_secs: 0,
                include_orphan_snapshots: false,
                dry_run: false,
                ..AutoGcOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.outcome, AutoGcOutcome::Ran);
        assert!(!session.exists(), "session expires under default policy");
        assert!(manual.exists(), "manual never age-expires by default");
    }

    #[test]
    fn env_dry_run_forces_dry_run_on_raw_opts() {
        // Raw AutoGcOptions dry_run=false must still dry-run when env is set.
        let _g = env_guard();
        clear_auto_gc_env();
        unsafe { std::env::set_var(ENV_AUTO_GC_DRY_RUN, "1") };
        let tmp = tempfile::TempDir::new().unwrap();
        let db = WorktreeDb::open(tmp.path()).unwrap();
        let dir = tmp.path().join("would-expire");
        std::fs::create_dir(&dir).unwrap();
        db.register(&make_rec("exp", dir.clone(), WorktreeKind::Session, 1))
            .unwrap();
        let report = maybe_auto_gc(&db, &opts_enabled_no_orphans(false)).unwrap();
        assert_eq!(report.outcome, AutoGcOutcome::Ran);
        assert!(
            dir.exists(),
            "env dry-run must not delete even when opts.dry_run=false"
        );
        assert!(report.age_expiry_enabled, "dry-run enables age metrics");
        clear_auto_gc_env();
    }

    #[test]
    fn dry_run_skips_orphan_cleaners() {
        let _g = env_guard();
        clear_auto_gc_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let db = WorktreeDb::open(tmp.path()).unwrap();
        let report = maybe_auto_gc(
            &db,
            &AutoGcOptions {
                min_interval_secs: 0,
                include_orphan_snapshots: true,
                dry_run: true,
                ..AutoGcOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.outcome, AutoGcOutcome::Ran);
        assert!(
            report.overlay.is_none() && report.btrfs.is_none(),
            "dry_run must not invoke orphan cleaners"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_dry_run_runs_orphan_cleaners_on_linux() {
        // Complementary to dry_run_skips_orphan_cleaners (Linux-only symbols).
        let _g = env_guard();
        clear_auto_gc_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let db = WorktreeDb::open(tmp.path()).unwrap();
        let report = maybe_auto_gc(
            &db,
            &AutoGcOptions {
                min_interval_secs: 0,
                include_orphan_snapshots: true,
                dry_run: false,
                ..AutoGcOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.outcome, AutoGcOutcome::Ran);
        assert!(
            report.overlay.is_some() && report.btrfs.is_some(),
            "non-dry-run + include_orphan_snapshots must invoke cleaners on Linux"
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_orphan_cleaners_always_absent() {
        // Orphan cleaners are compile-gated; non-Linux always returns None.
        let _g = env_guard();
        clear_auto_gc_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let db = WorktreeDb::open(tmp.path()).unwrap();
        let report = maybe_auto_gc(
            &db,
            &AutoGcOptions {
                min_interval_secs: 0,
                include_orphan_snapshots: true,
                dry_run: false,
                ..AutoGcOptions::default()
            },
        )
        .unwrap();
        assert!(report.overlay.is_none() && report.btrfs.is_none());
    }

    #[test]
    fn force_never_applied_by_auto_path_on_live_pid() {
        let _g = env_guard();
        clear_auto_gc_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let db = WorktreeDb::open(tmp.path()).unwrap();
        let dir = tmp.path().join("live-wt");
        std::fs::create_dir(&dir).unwrap();
        let mut rec = make_rec("live", dir.clone(), WorktreeKind::Session, 1);
        rec.creator_pid = Some(std::process::id());
        db.register(&rec).unwrap();

        let report = maybe_auto_gc(&db, &opts_enabled_no_orphans(true)).unwrap();
        assert_eq!(report.outcome, AutoGcOutcome::Ran);
        assert!(dir.exists());
        let gc = report.gc.unwrap();
        assert_eq!(gc.expired_removed, 0);
        assert_eq!(gc.skipped_alive, 1);
    }

    #[test]
    fn maybe_auto_gc_protects_process_cwd() {
        // Lock order: ENV_LOCK then CWD_TEST_LOCK.
        let _g = env_guard();
        let _cwd_lock = crate::api::cwd_test_guard();
        clear_auto_gc_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let db = WorktreeDb::open(tmp.path()).unwrap();
        let dir = tmp.path().join("cwd-wt");
        std::fs::create_dir(&dir).unwrap();
        db.register(&make_rec("cwd", dir.clone(), WorktreeKind::Session, 1))
            .unwrap();

        let _cwd = crate::api::CwdGuard(std::env::current_dir().unwrap());
        std::env::set_current_dir(&dir).unwrap();
        let report = maybe_auto_gc(
            &db,
            &AutoGcOptions {
                max_age_secs: 0,
                min_interval_secs: 0,
                include_orphan_snapshots: false,
                dry_run: true,
                ..AutoGcOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.outcome, AutoGcOutcome::Ran);
        let gc = report.gc.unwrap();
        assert_eq!(
            gc.expired_removed, 0,
            "process cwd inside wt must not count as would-expire"
        );
        assert!(
            gc.skipped_alive >= 1,
            "protect_paths must skip the cwd worktree"
        );
        assert!(dir.exists());
    }

    #[test]
    fn throttle_gc_ok_stamps_and_within_interval_throttles() {
        let _g = env_guard();
        clear_auto_gc_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let db = WorktreeDb::open(tmp.path()).unwrap();
        let opts = AutoGcOptions {
            min_interval_secs: 3600,
            include_orphan_snapshots: false,
            dry_run: false,
            ..AutoGcOptions::default()
        };
        assert!(db.get_meta(META_LAST_AUTO_GC_AT).unwrap().is_none());
        let first = maybe_auto_gc(&db, &opts).unwrap();
        assert_eq!(first.outcome, AutoGcOutcome::Ran);
        assert!(first.stamped);
        assert_eq!(
            first.age_expiry_enabled,
            process_cwd_scan_available(),
            "non-dry-run age_expiry tracks process_cwd_scan_available"
        );
        let stamp = db.get_meta(META_LAST_AUTO_GC_AT).unwrap();
        assert!(stamp.is_some(), "GC Ok must stamp last_auto_gc_at");

        let second = maybe_auto_gc(&db, &opts).unwrap();
        assert_eq!(second.outcome, AutoGcOutcome::Throttled);
        assert_eq!(
            db.get_meta(META_LAST_AUTO_GC_AT).unwrap(),
            stamp,
            "throttled pass must not rewrite stamp"
        );
    }

    #[test]
    fn future_stamp_is_treated_as_due() {
        let _g = env_guard();
        clear_auto_gc_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let db = WorktreeDb::open(tmp.path()).unwrap();
        // Far-future stamp must not black out auto-GC forever.
        db.set_meta(
            META_LAST_AUTO_GC_AT,
            &(now_epoch_secs() + 86_400).to_string(),
        )
        .unwrap();
        let report = maybe_auto_gc(&db, &opts_enabled_no_orphans(false)).unwrap();
        assert_eq!(report.outcome, AutoGcOutcome::Ran);
    }

    #[test]
    fn is_throttled_logic() {
        assert!(!is_throttled(1000, 2000, 3600), "future stamp is due");
        assert!(is_throttled(1000, 900, 3600), "within interval");
        assert!(!is_throttled(5000, 1000, 3600), "past interval");
        assert!(
            !is_throttled(1000, 1000, 0),
            "zero interval never throttles"
        );
    }

    #[test]
    fn throttle_gc_err_does_not_stamp() {
        let _g = env_guard();
        clear_auto_gc_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let db = WorktreeDb::open(tmp.path()).unwrap();
        db.execute_batch_for_test("DROP TABLE worktrees;").unwrap();
        let err = maybe_auto_gc(&db, &opts_enabled_no_orphans(false));
        assert!(err.is_err(), "GC Err must surface as Err");
        assert!(
            db.get_meta(META_LAST_AUTO_GC_AT).unwrap().is_none(),
            "GC Err must not stamp last_auto_gc_at"
        );
    }

    #[test]
    fn throttle_stamps_even_when_remove_failed() {
        // remove_failed > 0 still stamps (partial progress).
        let _g = env_guard();
        clear_auto_gc_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let db = WorktreeDb::open(tmp.path()).unwrap();
        let path = tmp.path().join("doomed");
        std::fs::write(&path, b"not a dir").unwrap();
        db.register(&make_rec("doomed", path, WorktreeKind::Session, 1))
            .unwrap();

        let opts = AutoGcOptions {
            max_age_secs: 0,
            min_interval_secs: 0,
            include_orphan_snapshots: false,
            dry_run: false,
            ..AutoGcOptions::default()
        };
        let report = maybe_auto_gc(&db, &opts).unwrap();
        assert_eq!(report.outcome, AutoGcOutcome::Ran);
        assert!(
            db.get_meta(META_LAST_AUTO_GC_AT).unwrap().is_some(),
            "Ran must stamp even with remove_failed / dead-only"
        );
        assert_eq!(
            report.age_expiry_enabled,
            process_cwd_scan_available(),
            "non-dry-run age_expiry tracks platform CWD scan"
        );
        if process_cwd_scan_available() {
            let gc = report.gc.as_ref().unwrap();
            assert!(
                gc.remove_failed >= 1,
                "age path must record remove_failed on non-dir"
            );
        }
    }

    #[test]
    fn disabled_env_wins_even_if_opts_enabled() {
        let _g = env_guard();
        clear_auto_gc_env();
        unsafe { std::env::set_var(ENV_AUTO_GC, "0") };
        let tmp = tempfile::TempDir::new().unwrap();
        let db = WorktreeDb::open(tmp.path()).unwrap();
        let report = maybe_auto_gc(&db, &opts_enabled_no_orphans(false)).unwrap();
        assert_eq!(report.outcome, AutoGcOutcome::Disabled);
        assert!(db.get_meta(META_LAST_AUTO_GC_AT).unwrap().is_none());
        clear_auto_gc_env();
    }

    #[test]
    fn env_kill_truthy_falsy_table() {
        let _g = env_guard();
        for (val, disabled) in [
            ("0", true),
            ("false", true),
            ("FALSE", true),
            ("off", true),
            ("no", true),
            ("disabled", true),
            ("", true),
            ("1", false),
            ("true", false),
            ("on", false),
            ("yes", false),
            ("enabled", false),
        ] {
            clear_auto_gc_env();
            unsafe { std::env::set_var(ENV_AUTO_GC, val) };
            assert_eq!(
                env_auto_gc_disabled(),
                disabled,
                "ENV_AUTO_GC={val:?} disabled={disabled}"
            );
        }
        clear_auto_gc_env();
        assert!(!env_auto_gc_disabled(), "unset is not disabled");
    }

    #[test]
    fn env_dry_run_truthy_table() {
        let _g = env_guard();
        for (val, on) in [
            ("1", true),
            ("true", true),
            ("yes", true),
            ("on", true),
            ("enabled", true),
            ("0", false),
            ("false", false),
            ("", false),
            ("nope", false),
        ] {
            clear_auto_gc_env();
            unsafe { std::env::set_var(ENV_AUTO_GC_DRY_RUN, val) };
            assert_eq!(
                env_auto_gc_dry_run(),
                on,
                "ENV_AUTO_GC_DRY_RUN={val:?} on={on}"
            );
        }
        clear_auto_gc_env();
        assert!(!env_auto_gc_dry_run());
    }

    #[test]
    fn disabled_opts_returns_disabled() {
        let _g = env_guard();
        clear_auto_gc_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let db = WorktreeDb::open(tmp.path()).unwrap();
        let report = maybe_auto_gc(
            &db,
            &AutoGcOptions {
                enabled: false,
                min_interval_secs: 0,
                include_orphan_snapshots: false,
                ..AutoGcOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.outcome, AutoGcOutcome::Disabled);
        assert!(db.get_meta(META_LAST_AUTO_GC_AT).unwrap().is_none());
    }

    #[test]
    fn complementary_enabled_runs_when_env_unset() {
        let _g = env_guard();
        clear_auto_gc_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let db = WorktreeDb::open(tmp.path()).unwrap();
        let report = maybe_auto_gc(&db, &opts_enabled_no_orphans(false)).unwrap();
        assert_eq!(report.outcome, AutoGcOutcome::Ran);
        assert!(db.get_meta(META_LAST_AUTO_GC_AT).unwrap().is_some());
    }

    #[test]
    fn fail_closed_meta_read_returns_err_without_stamp() {
        let _g = env_guard();
        clear_auto_gc_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let db = WorktreeDb::open(tmp.path()).unwrap();
        db.execute_batch_for_test("DROP TABLE meta;").unwrap();
        let err = maybe_auto_gc(&db, &opts_enabled_no_orphans(false));
        assert!(err.is_err(), "meta read failure must fail closed");
    }

    #[test]
    fn auto_path_dead_reclaim_includes_manual_kind() {
        // never-expire is age-only; dead Manual still unregisters.
        let _g = env_guard();
        clear_auto_gc_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let db = WorktreeDb::open(tmp.path()).unwrap();
        db.register(&make_rec(
            "manual-dead",
            PathBuf::from("/nonexistent/manual-wt"),
            WorktreeKind::Manual,
            100,
        ))
        .unwrap();
        let report = maybe_auto_gc(&db, &opts_enabled_no_orphans(false)).unwrap();
        assert_eq!(report.outcome, AutoGcOutcome::Ran);
        assert_eq!(report.gc.as_ref().unwrap().dead_removed, 1);
        let all = db
            .list(&ListFilter {
                include_dead: true,
                ..Default::default()
            })
            .unwrap();
        assert!(all.is_empty());
    }

    #[test]
    fn resolve_layers_env_kill_and_local_and_clamps() {
        let _g = env_guard();
        clear_auto_gc_env();

        let local_off = WorktreeAutoGcLayer {
            enabled: Some(false),
            ..Default::default()
        };
        let remote_on = WorktreeAutoGcLayer {
            enabled: Some(true),
            max_age_secs: Some(1),
            min_interval_secs: Some(1),
            dry_run: Some(false),
            ..Default::default()
        };
        let p = resolve_worktree_auto_gc_from_layers(Some(&local_off), Some(&remote_on));
        assert!(!p.enabled, "local enabled=false beats remote true");
        assert_eq!(p.max_age_secs, MAX_AGE_SECS_MIN, "remote TTL still clamped");
        assert_eq!(p.max_age_by_kind.get(&WorktreeKind::Manual), Some(&None));

        unsafe { std::env::set_var(ENV_AUTO_GC, "0") };
        let p = resolve_worktree_auto_gc_from_layers(
            Some(&WorktreeAutoGcLayer {
                enabled: Some(true),
                ..Default::default()
            }),
            None,
        );
        assert!(!p.enabled, "env kill wins");
        clear_auto_gc_env();

        unsafe { std::env::set_var(ENV_AUTO_GC_DRY_RUN, "1") };
        let p = resolve_worktree_auto_gc_from_layers(
            Some(&WorktreeAutoGcLayer {
                dry_run: Some(false),
                ..Default::default()
            }),
            Some(&WorktreeAutoGcLayer {
                dry_run: Some(false),
                ..Default::default()
            }),
        );
        assert!(p.dry_run, "env dry-run wins over local/remote false");
        clear_auto_gc_env();
    }

    #[test]
    fn resolve_kind_map_local_wins_over_remote_and_clamps() {
        let _g = env_guard();
        clear_auto_gc_env();
        let remote = WorktreeAutoGcLayer {
            max_age_by_kind: BTreeMap::from([
                (WorktreeKind::Subagent, Some(1)), // clamps to MIN
                (WorktreeKind::Manual, Some(86400)),
                (WorktreeKind::Pool, Some(172800)),
            ]),
            ..Default::default()
        };
        let local = WorktreeAutoGcLayer {
            max_age_by_kind: BTreeMap::from([
                (WorktreeKind::Subagent, Some(7200)),
                // local omits manual → remote's manual expire stays
            ]),
            ..Default::default()
        };
        let p = resolve_worktree_auto_gc_from_layers(Some(&local), Some(&remote));
        assert_eq!(
            p.max_age_by_kind.get(&WorktreeKind::Subagent),
            Some(&Some(7200)),
            "local kind TTL wins"
        );
        assert_eq!(
            p.max_age_by_kind.get(&WorktreeKind::Manual),
            Some(&Some(86400)),
            "remote can make manual expire when local omits"
        );
        assert_eq!(
            p.max_age_by_kind.get(&WorktreeKind::Pool),
            Some(&Some(172800))
        );

        // Local can restore manual never.
        let local_never = WorktreeAutoGcLayer {
            max_age_by_kind: BTreeMap::from([(WorktreeKind::Manual, None)]),
            ..Default::default()
        };
        let p = resolve_worktree_auto_gc_from_layers(Some(&local_never), Some(&remote));
        assert_eq!(p.max_age_by_kind.get(&WorktreeKind::Manual), Some(&None));
    }

    #[test]
    fn resolve_env_max_age_wins_over_local_and_remote() {
        let _g = env_guard();
        clear_auto_gc_env();
        unsafe { std::env::set_var(ENV_AUTO_GC_MAX_AGE, "7200") };
        let local = WorktreeAutoGcLayer {
            max_age_secs: Some(86400),
            ..Default::default()
        };
        let remote = WorktreeAutoGcLayer {
            max_age_secs: Some(3600),
            ..Default::default()
        };
        let p = resolve_worktree_auto_gc_from_layers(Some(&local), Some(&remote));
        assert_eq!(p.max_age_secs, 7200, "env MAX_AGE wins");
        clear_auto_gc_env();

        unsafe { std::env::set_var(ENV_AUTO_GC_MAX_AGE, "not-a-number") };
        let p = resolve_worktree_auto_gc_from_layers(Some(&local), None);
        assert_eq!(
            p.max_age_secs, 86400,
            "invalid env max age falls through to local"
        );
        clear_auto_gc_env();
    }

    #[test]
    fn resolve_defaults_include_manual_never() {
        let _g = env_guard();
        clear_auto_gc_env();
        let p = ResolvedWorktreeAutoGc::from_env_only();
        assert_eq!(p.max_age_by_kind, default_max_age_by_kind());
        assert_eq!(p.max_age_secs, DEFAULT_MAX_AGE_SECS);
        assert!(
            !p.include_rebuild,
            "rebuild off by default until cost measured"
        );
        assert_eq!(
            p.rebuild_min_interval_secs,
            DEFAULT_REBUILD_MIN_INTERVAL_SECS
        );
    }

    #[test]
    fn resolve_env_rebuild_enables_include_rebuild() {
        let _g = env_guard();
        clear_auto_gc_env();
        unsafe { std::env::set_var(ENV_AUTO_GC_REBUILD, "1") };
        let p = ResolvedWorktreeAutoGc::from_env_only();
        assert!(
            p.include_rebuild,
            "env REBUILD=1 must enable include_rebuild"
        );
        clear_auto_gc_env();

        let local_off = WorktreeAutoGcLayer {
            include_rebuild: Some(false),
            ..Default::default()
        };
        unsafe { std::env::set_var(ENV_AUTO_GC_REBUILD, "1") };
        let p = resolve_worktree_auto_gc_from_layers(Some(&local_off), None);
        assert!(p.include_rebuild, "env REBUILD=1 wins over local false");
        clear_auto_gc_env();
    }

    #[test]
    fn resolve_local_include_rebuild_and_interval() {
        let _g = env_guard();
        clear_auto_gc_env();
        let local = WorktreeAutoGcLayer {
            include_rebuild: Some(true),
            rebuild_min_interval_secs: Some(120),
            ..Default::default()
        };
        let p = resolve_worktree_auto_gc_from_layers(Some(&local), None);
        assert!(p.include_rebuild);
        assert_eq!(p.rebuild_min_interval_secs, 120);
    }

    #[test]
    fn from_env_only_honors_dry_run_env() {
        let _g = env_guard();
        clear_auto_gc_env();
        unsafe { std::env::set_var(ENV_AUTO_GC_DRY_RUN, "1") };
        let p = ResolvedWorktreeAutoGc::from_env_only();
        assert!(p.dry_run);
        assert!(p.enabled);
        clear_auto_gc_env();
    }

    #[test]
    fn clamps_numeric_bounds() {
        assert_eq!(clamp_max_age_secs(1), MAX_AGE_SECS_MIN);
        assert_eq!(clamp_max_age_secs(u64::MAX), MAX_AGE_SECS_MAX);
        assert_eq!(clamp_max_age_secs(604800), 604800);
        assert_eq!(clamp_min_interval_secs(1), MIN_INTERVAL_SECS_MIN);
        assert_eq!(clamp_min_interval_secs(u64::MAX), MIN_INTERVAL_SECS_MAX);
    }

    #[test]
    fn get_set_meta_round_trip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = WorktreeDb::open(tmp.path()).unwrap();
        assert_eq!(db.get_meta("k").unwrap(), None);
        db.set_meta("k", "v").unwrap();
        assert_eq!(db.get_meta("k").unwrap().as_deref(), Some("v"));
        db.set_meta("k", "v2").unwrap();
        assert_eq!(db.get_meta("k").unwrap().as_deref(), Some("v2"));
    }

    #[test]
    fn set_meta_err_after_gc_still_returns_ran() {
        // Stamp write failure must not turn a successful GC into Err for hooks.
        let _g = env_guard();
        clear_auto_gc_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let db = WorktreeDb::open(tmp.path()).unwrap();
        db.execute_batch_for_test(
            "CREATE TRIGGER block_meta_write BEFORE INSERT ON meta BEGIN
               SELECT RAISE(ABORT, 'blocked');
             END;",
        )
        .unwrap();
        let report = maybe_auto_gc(&db, &opts_enabled_no_orphans(false)).unwrap();
        assert_eq!(
            report.outcome,
            AutoGcOutcome::Ran,
            "set_meta Err after GC Ok must still return Ok(Ran)"
        );
        assert!(!report.stamped, "failed set_meta must report stamped=false");
    }

    #[test]
    fn unparseable_stamp_fails_open_and_restamps() {
        let _g = env_guard();
        clear_auto_gc_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let db = WorktreeDb::open(tmp.path()).unwrap();
        db.set_meta(META_LAST_AUTO_GC_AT, "not-a-number").unwrap();
        let report = maybe_auto_gc(&db, &opts_enabled_no_orphans(false)).unwrap();
        assert_eq!(report.outcome, AutoGcOutcome::Ran);
        assert!(report.stamped);
        let stamp = db.get_meta(META_LAST_AUTO_GC_AT).unwrap().unwrap();
        assert!(
            stamp.parse::<i64>().is_ok(),
            "must restamp a parseable epoch after unparseable prior value"
        );
    }

    #[test]
    fn include_rebuild_false_skips_rebuild_and_does_not_stamp_rebuild_meta() {
        let _g = env_guard();
        clear_auto_gc_env();
        let fx = crate::db::GrokHomeFixture::new();
        let db = WorktreeDb::open(&fx.home).unwrap();

        let wt = fx.home.join("worktrees/repo/untracked-sess");
        std::fs::create_dir_all(wt.join(".git")).unwrap();

        let report = maybe_auto_gc(
            &db,
            &AutoGcOptions {
                min_interval_secs: 0,
                include_orphan_snapshots: false,
                include_rebuild: false,
                ..AutoGcOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.outcome, AutoGcOutcome::Ran);
        assert!(report.rebuild.is_none());
        assert!(!report.rebuild_stamped);
        assert_eq!(report.stale_registrations_cleaned, 0);
        assert!(
            db.get_meta(META_LAST_AUTO_REBUILD_AT).unwrap().is_none(),
            "include_rebuild=false must not stamp last_auto_rebuild_at"
        );
        assert!(
            db.get(&wt.to_string_lossy()).unwrap().is_none(),
            "untracked dir must not be registered when rebuild disabled"
        );
    }

    #[test]
    fn include_rebuild_true_registers_untracked_under_grok_home() {
        let _g = env_guard();
        clear_auto_gc_env();
        let fx = crate::db::GrokHomeFixture::new();
        let db = WorktreeDb::open(&fx.home).unwrap();

        let wt = fx.home.join("worktrees/repo/untracked-sess");
        std::fs::create_dir_all(wt.join(".git")).unwrap();

        let report = maybe_auto_gc(
            &db,
            &AutoGcOptions {
                min_interval_secs: 0,
                include_orphan_snapshots: false,
                include_rebuild: true,
                rebuild_min_interval_secs: 0,
                ..AutoGcOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.outcome, AutoGcOutcome::Ran);
        let rebuild = report
            .rebuild
            .as_ref()
            .expect("rebuild must run when enabled and due");
        assert_eq!(rebuild.discovered, 1);
        assert_eq!(rebuild.registered, 1);
        assert!(report.rebuild_stamped);
        assert!(
            db.get_meta(META_LAST_AUTO_REBUILD_AT).unwrap().is_some(),
            "successful rebuild must stamp last_auto_rebuild_at"
        );
        assert!(
            db.get(&wt.to_string_lossy()).unwrap().is_some(),
            "untracked dir under grok_home/worktrees must be registered"
        );
    }

    #[test]
    fn rebuild_throttled_independently_of_gc() {
        let _g = env_guard();
        clear_auto_gc_env();
        let fx = crate::db::GrokHomeFixture::new();
        let db = WorktreeDb::open(&fx.home).unwrap();

        let opts = AutoGcOptions {
            min_interval_secs: 0, // GC always due
            include_orphan_snapshots: false,
            include_rebuild: true,
            rebuild_min_interval_secs: 3600,
            ..AutoGcOptions::default()
        };

        let first = maybe_auto_gc(&db, &opts).unwrap();
        assert_eq!(first.outcome, AutoGcOutcome::Ran);
        assert!(first.rebuild.is_some());
        assert!(first.rebuild_stamped);
        let rebuild_stamp = db.get_meta(META_LAST_AUTO_REBUILD_AT).unwrap();
        assert!(rebuild_stamp.is_some());

        // Second GC pass still runs (min_interval 0) but rebuild is throttled.
        let second = maybe_auto_gc(&db, &opts).unwrap();
        assert_eq!(second.outcome, AutoGcOutcome::Ran);
        assert!(
            second.rebuild.is_none(),
            "rebuild within rebuild_min_interval must skip"
        );
        assert!(!second.rebuild_stamped);
        assert_eq!(
            db.get_meta(META_LAST_AUTO_REBUILD_AT).unwrap(),
            rebuild_stamp,
            "throttled rebuild must not rewrite stamp"
        );
        assert!(
            second.stamped,
            "GC stamp still advances when rebuild is throttled"
        );
    }

    #[test]
    fn rebuild_failure_does_not_block_dead_record_gc() {
        // INSERT-aborting trigger makes rebuild register fail; SELECT/DELETE for
        // dead-path GC still work so reclaim continues after rebuild Err.
        let _g = env_guard();
        clear_auto_gc_env();
        let fx = crate::db::GrokHomeFixture::new();
        let db = WorktreeDb::open(&fx.home).unwrap();

        db.register(&make_rec(
            "dead-after-rebuild-err",
            PathBuf::from("/nonexistent/dead-wt-rebuild-err"),
            WorktreeKind::Session,
            100,
        ))
        .unwrap();

        let untracked = fx.home.join("worktrees/repo/untracked-for-fail");
        std::fs::create_dir_all(untracked.join(".git")).unwrap();

        db.execute_batch_for_test(
            "CREATE TRIGGER block_worktree_insert BEFORE INSERT ON worktrees BEGIN
               SELECT RAISE(ABORT, 'rebuild-blocked');
             END;",
        )
        .unwrap();

        let report = maybe_auto_gc(
            &db,
            &AutoGcOptions {
                min_interval_secs: 0,
                include_orphan_snapshots: false,
                include_rebuild: true,
                rebuild_min_interval_secs: 0,
                ..AutoGcOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.outcome, AutoGcOutcome::Ran);
        assert!(
            report.rebuild.is_none(),
            "rebuild Err must not populate rebuild report"
        );
        assert!(
            !report.rebuild_stamped,
            "failed rebuild must not stamp last_auto_rebuild_at"
        );
        assert!(
            db.get_meta(META_LAST_AUTO_REBUILD_AT).unwrap().is_none(),
            "failed rebuild must leave rebuild meta unset"
        );
        assert_eq!(
            report.gc.as_ref().unwrap().dead_removed,
            1,
            "dead-record GC must still run when rebuild fails"
        );
        assert!(report.stamped, "GC Ok must still stamp last_auto_gc_at");
    }

    #[test]
    fn dry_run_skips_rebuild_and_prune() {
        let _g = env_guard();
        clear_auto_gc_env();
        let fx = crate::db::GrokHomeFixture::new();
        let db = WorktreeDb::open(&fx.home).unwrap();
        let wt = fx.home.join("worktrees/repo/dry-sess");
        std::fs::create_dir_all(wt.join(".git")).unwrap();

        let report = maybe_auto_gc(
            &db,
            &AutoGcOptions {
                min_interval_secs: 0,
                include_orphan_snapshots: false,
                include_rebuild: true,
                rebuild_min_interval_secs: 0,
                dry_run: true,
                ..AutoGcOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.outcome, AutoGcOutcome::Ran);
        assert!(report.rebuild.is_none());
        assert!(!report.rebuild_stamped);
        assert_eq!(report.stale_registrations_cleaned, 0);
        assert!(db.get_meta(META_LAST_AUTO_REBUILD_AT).unwrap().is_none());
        assert!(db.get(&wt.to_string_lossy()).unwrap().is_none());
    }

    #[test]
    fn prune_removes_stale_git_worktree_registration() {
        let _g = env_guard();
        clear_auto_gc_env();
        let fx = crate::db::GrokHomeFixture::new();
        let db = WorktreeDb::open(&fx.home).unwrap();

        let repo = fx.home.join("src-repo");
        let wt = fx.home.join("linked-wt");
        plant_stale_git_worktree(&repo, &wt);
        let before = count_regs(&repo);
        assert!(before >= 1, "expected stale registration before prune");

        let tracked = fx.home.join("still-there");
        std::fs::create_dir_all(&tracked).unwrap();
        let mut rec = make_rec("tracked", tracked, WorktreeKind::Session, now_epoch_secs());
        rec.source_repo = repo.clone();
        db.register(&rec).unwrap();

        let report = maybe_auto_gc(
            &db,
            &AutoGcOptions {
                min_interval_secs: 0,
                include_orphan_snapshots: false,
                include_rebuild: true,
                rebuild_min_interval_secs: 0,
                dry_run: false,
                ..AutoGcOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.outcome, AutoGcOutcome::Ran);
        assert!(
            report.stale_registrations_cleaned >= 1,
            "stale registration must be pruned; cleaned={}",
            report.stale_registrations_cleaned
        );
        assert!(
            count_regs(&repo) < before,
            "registration count must drop after prune"
        );
    }

    #[test]
    fn prune_noop_when_no_stale_registrations() {
        let _g = env_guard();
        clear_auto_gc_env();
        let fx = crate::db::GrokHomeFixture::new();
        let db = WorktreeDb::open(&fx.home).unwrap();

        let repo = fx.home.join("clean-repo");
        std::fs::create_dir_all(&repo).unwrap();
        let init = std::process::Command::new("git")
            .args(["init"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(init.status.success());

        let path = fx.home.join("alive-wt");
        std::fs::create_dir_all(&path).unwrap();
        let mut rec = make_rec("alive", path, WorktreeKind::Session, now_epoch_secs());
        rec.source_repo = repo;
        db.register(&rec).unwrap();

        let report = maybe_auto_gc(
            &db,
            &AutoGcOptions {
                min_interval_secs: 0,
                include_orphan_snapshots: false,
                include_rebuild: true,
                rebuild_min_interval_secs: 0,
                ..AutoGcOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.outcome, AutoGcOutcome::Ran);
        assert_eq!(
            report.stale_registrations_cleaned, 0,
            "clean repo must report zero stale prunes"
        );
    }

    #[test]
    fn dry_run_skips_prune_even_with_stale_registration() {
        let _g = env_guard();
        clear_auto_gc_env();
        let fx = crate::db::GrokHomeFixture::new();
        let db = WorktreeDb::open(&fx.home).unwrap();
        let repo = fx.home.join("dry-src");
        let wt = fx.home.join("dry-stale-wt");
        plant_stale_git_worktree(&repo, &wt);
        let before = count_regs(&repo);
        assert!(before >= 1);

        let tracked = fx.home.join("dry-tracked");
        std::fs::create_dir_all(&tracked).unwrap();
        let mut rec = make_rec("dry-t", tracked, WorktreeKind::Session, now_epoch_secs());
        rec.source_repo = repo.clone();
        db.register(&rec).unwrap();

        let report = maybe_auto_gc(
            &db,
            &AutoGcOptions {
                min_interval_secs: 0,
                include_orphan_snapshots: false,
                include_rebuild: true,
                rebuild_min_interval_secs: 0,
                dry_run: true,
                ..AutoGcOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.stale_registrations_cleaned, 0);
        assert_eq!(count_regs(&repo), before, "dry_run must not prune");
    }

    #[test]
    fn include_rebuild_false_skips_prune_even_with_stale_registration() {
        let _g = env_guard();
        clear_auto_gc_env();
        let fx = crate::db::GrokHomeFixture::new();
        let db = WorktreeDb::open(&fx.home).unwrap();
        let repo = fx.home.join("off-src");
        let wt = fx.home.join("off-stale-wt");
        plant_stale_git_worktree(&repo, &wt);
        let before = count_regs(&repo);
        assert!(before >= 1);

        let tracked = fx.home.join("off-tracked");
        std::fs::create_dir_all(&tracked).unwrap();
        let mut rec = make_rec("off-t", tracked, WorktreeKind::Session, now_epoch_secs());
        rec.source_repo = repo.clone();
        db.register(&rec).unwrap();

        let report = maybe_auto_gc(
            &db,
            &AutoGcOptions {
                min_interval_secs: 0,
                include_orphan_snapshots: false,
                include_rebuild: false,
                ..AutoGcOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.stale_registrations_cleaned, 0);
        assert_eq!(
            count_regs(&repo),
            before,
            "include_rebuild=false must not prune"
        );
    }

    #[test]
    fn prune_uses_dead_row_source_repo_snapshot() {
        // Sole tracked row is dead (path gone); after GC unregisters it, prune must
        // still hit the snapshotted source_repo.
        let _g = env_guard();
        clear_auto_gc_env();
        let fx = crate::db::GrokHomeFixture::new();
        let db = WorktreeDb::open(&fx.home).unwrap();
        let repo = fx.home.join("dead-src");
        let wt = fx.home.join("dead-stale-wt");
        plant_stale_git_worktree(&repo, &wt);
        let before = count_regs(&repo);
        assert!(before >= 1);

        let mut rec = make_rec(
            "sole-dead",
            PathBuf::from("/nonexistent/sole-dead-wt"),
            WorktreeKind::Session,
            100,
        );
        rec.source_repo = repo.clone();
        db.register(&rec).unwrap();

        let report = maybe_auto_gc(
            &db,
            &AutoGcOptions {
                min_interval_secs: 0,
                include_orphan_snapshots: false,
                include_rebuild: true,
                rebuild_min_interval_secs: 0,
                ..AutoGcOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.gc.as_ref().unwrap().dead_removed, 1);
        assert!(
            report.stale_registrations_cleaned >= 1,
            "dead sole-row source_repo must still be pruned; cleaned={}",
            report.stale_registrations_cleaned
        );
        assert!(count_regs(&repo) < before);
    }

    #[test]
    fn rebuild_unparseable_stamp_fails_open() {
        let _g = env_guard();
        clear_auto_gc_env();
        let fx = crate::db::GrokHomeFixture::new();
        let db = WorktreeDb::open(&fx.home).unwrap();
        db.set_meta(META_LAST_AUTO_REBUILD_AT, "not-a-number")
            .unwrap();
        let wt = fx.home.join("worktrees/repo/reparse-sess");
        std::fs::create_dir_all(wt.join(".git")).unwrap();

        let report = maybe_auto_gc(
            &db,
            &AutoGcOptions {
                min_interval_secs: 0,
                include_orphan_snapshots: false,
                include_rebuild: true,
                rebuild_min_interval_secs: 3600,
                ..AutoGcOptions::default()
            },
        )
        .unwrap();
        assert!(
            report.rebuild.is_some(),
            "unparseable rebuild stamp must not throttle"
        );
        assert!(report.rebuild_stamped);
        let stamp = db.get_meta(META_LAST_AUTO_REBUILD_AT).unwrap().unwrap();
        assert!(stamp.parse::<i64>().is_ok());
    }

    #[test]
    fn rebuild_set_meta_failure_still_continues_gc() {
        let _g = env_guard();
        clear_auto_gc_env();
        let fx = crate::db::GrokHomeFixture::new();
        let db = WorktreeDb::open(&fx.home).unwrap();
        db.register(&make_rec(
            "dead-stamp",
            PathBuf::from("/nonexistent/dead-stamp-wt"),
            WorktreeKind::Session,
            100,
        ))
        .unwrap();
        // Block only INSERT (UPSERT is INSERT OR REPLACE → INSERT path).
        db.execute_batch_for_test(
            "CREATE TRIGGER block_meta_insert BEFORE INSERT ON meta BEGIN
               SELECT RAISE(ABORT, 'meta-blocked');
             END;",
        )
        .unwrap();

        let report = maybe_auto_gc(
            &db,
            &AutoGcOptions {
                min_interval_secs: 0,
                include_orphan_snapshots: false,
                include_rebuild: true,
                rebuild_min_interval_secs: 0,
                ..AutoGcOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.outcome, AutoGcOutcome::Ran);
        // Rebuild may succeed registering before stamp; both stamps use set_meta INSERT.
        if report.rebuild.is_some() {
            assert!(!report.rebuild_stamped);
        }
        assert_eq!(
            report.gc.as_ref().unwrap().dead_removed,
            1,
            "GC must continue after rebuild stamp failure"
        );
        assert!(!report.stamped, "GC stamp also uses set_meta INSERT");
    }

    #[test]
    fn rebuild_not_stamped_when_gc_fails_after_rebuild() {
        // Rebuild succeeds (registers untracked), then GC fails on sweep UPDATE.
        // Rebuild meta must stay unset so the next pass can re-discover.
        let _g = env_guard();
        clear_auto_gc_env();
        let fx = crate::db::GrokHomeFixture::new();
        let db = WorktreeDb::open(&fx.home).unwrap();
        db.register(&make_rec(
            "alive-missing-path",
            PathBuf::from("/nonexistent/alive-for-gc-fail"),
            WorktreeKind::Session,
            100,
        ))
        .unwrap();
        let untracked = fx.home.join("worktrees/repo/rebuild-then-gc-fail");
        std::fs::create_dir_all(untracked.join(".git")).unwrap();

        db.execute_batch_for_test(
            "CREATE TRIGGER block_worktree_update BEFORE UPDATE ON worktrees BEGIN
               SELECT RAISE(ABORT, 'gc-sweep-blocked');
             END;",
        )
        .unwrap();

        let err = maybe_auto_gc(
            &db,
            &AutoGcOptions {
                min_interval_secs: 0,
                include_orphan_snapshots: false,
                include_rebuild: true,
                rebuild_min_interval_secs: 0,
                ..AutoGcOptions::default()
            },
        )
        .expect_err("GC sweep UPDATE must fail the pass");
        let _ = err;
        assert!(
            db.get_meta(META_LAST_AUTO_REBUILD_AT).unwrap().is_none(),
            "rebuild must not stamp when GC fails after a successful rebuild"
        );
        assert!(
            db.get_meta(META_LAST_AUTO_GC_AT).unwrap().is_none(),
            "GC must not stamp when the pass returns Err"
        );
        // Rebuild already registered the untracked tree before GC failed.
        assert!(
            db.get(untracked.to_str().unwrap()).unwrap().is_some(),
            "rebuild registration from the failed pass is retained"
        );
    }

    #[test]
    fn classify_rebuild_meta_err_skips_not_aborts() {
        let err = Err(anyhow::anyhow!("meta unavailable"));
        assert_eq!(
            classify_rebuild_meta(err, 1000, 3600),
            RebuildMetaClass::SkipFailed
        );
        assert_eq!(
            classify_rebuild_meta(Ok(None), 1000, 3600),
            RebuildMetaClass::Due
        );
        assert_eq!(
            classify_rebuild_meta(Ok(Some("900".into())), 1000, 3600),
            RebuildMetaClass::Throttled
        );
        assert_eq!(
            classify_rebuild_meta(Ok(Some("not-a-number".into())), 1000, 3600),
            RebuildMetaClass::Due
        );
        assert_eq!(
            classify_rebuild_meta(Ok(Some("100".into())), 10000, 3600),
            RebuildMetaClass::Due
        );
    }

    #[test]
    fn env_rebuild_reapplied_inside_maybe_auto_gc() {
        let _g = env_guard();
        clear_auto_gc_env();
        unsafe { std::env::set_var(ENV_AUTO_GC_REBUILD, "1") };
        let fx = crate::db::GrokHomeFixture::new();
        let db = WorktreeDb::open(&fx.home).unwrap();
        let wt = fx.home.join("worktrees/repo/env-rebuild-sess");
        std::fs::create_dir_all(wt.join(".git")).unwrap();

        // opts.include_rebuild false — env must still enable.
        let report = maybe_auto_gc(
            &db,
            &AutoGcOptions {
                min_interval_secs: 0,
                include_orphan_snapshots: false,
                include_rebuild: false,
                rebuild_min_interval_secs: 0,
                ..AutoGcOptions::default()
            },
        )
        .unwrap();
        assert!(
            report.rebuild.is_some(),
            "env REBUILD must re-apply inside maybe_auto_gc"
        );
        clear_auto_gc_env();
    }

    #[test]
    fn gc_throttled_short_circuits_rebuild() {
        let _g = env_guard();
        clear_auto_gc_env();
        let fx = crate::db::GrokHomeFixture::new();
        let db = WorktreeDb::open(&fx.home).unwrap();
        // GC recently stamped; rebuild never stamped and would be due.
        db.set_meta(META_LAST_AUTO_GC_AT, &now_epoch_secs().to_string())
            .unwrap();
        let wt = fx.home.join("worktrees/repo/throttle-rebuild");
        std::fs::create_dir_all(wt.join(".git")).unwrap();

        let report = maybe_auto_gc(
            &db,
            &AutoGcOptions {
                min_interval_secs: 3600,
                include_orphan_snapshots: false,
                include_rebuild: true,
                rebuild_min_interval_secs: 0,
                ..AutoGcOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.outcome, AutoGcOutcome::Throttled);
        assert!(report.rebuild.is_none());
        assert!(
            db.get(&wt.to_string_lossy()).unwrap().is_none(),
            "GC throttle must skip rebuild"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn rebuild_same_pass_does_not_age_expire_new_registration() {
        let _g = env_guard();
        let _cwd_lock = crate::api::cwd_test_guard();
        clear_auto_gc_env();
        let fx = crate::db::GrokHomeFixture::new();
        let db = WorktreeDb::open(&fx.home).unwrap();
        let wt = fx.home.join("worktrees/repo/fresh-rebuild");
        std::fs::create_dir_all(wt.join(".git")).unwrap();
        // Old directory mtime would look expired under max_age=0 without touch.
        let report = maybe_auto_gc(
            &db,
            &AutoGcOptions {
                max_age_secs: 0,
                min_interval_secs: 0,
                include_orphan_snapshots: false,
                include_rebuild: true,
                rebuild_min_interval_secs: 0,
                dry_run: false,
                ..AutoGcOptions::default()
            },
        )
        .unwrap();
        assert!(report.age_expiry_enabled);
        assert!(report.rebuild.as_ref().is_some_and(|r| r.registered == 1));
        assert!(
            wt.exists(),
            "just-registered rebuild path must not age-delete same pass"
        );
        assert!(db.get(&wt.to_string_lossy()).unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn rebuild_refuses_symlink_escape_then_age_safe() {
        let _g = env_guard();
        clear_auto_gc_env();
        let fx = crate::db::GrokHomeFixture::new();
        let db = WorktreeDb::open(&fx.home).unwrap();
        let outside = fx.home.join("outside-escape");
        std::fs::create_dir_all(outside.join(".git")).unwrap();
        let parent = fx.home.join("worktrees/repo");
        std::fs::create_dir_all(&parent).unwrap();
        std::os::unix::fs::symlink(&outside, parent.join("escaped")).unwrap();

        let report = maybe_auto_gc(
            &db,
            &AutoGcOptions {
                min_interval_secs: 0,
                include_orphan_snapshots: false,
                include_rebuild: true,
                rebuild_min_interval_secs: 0,
                ..AutoGcOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.outcome, AutoGcOutcome::Ran);
        assert!(
            report.rebuild.as_ref().is_some_and(|r| r.registered == 0),
            "symlink escape must not register"
        );
        assert!(
            outside.exists(),
            "outside target must remain (never registered/deleted)"
        );
        assert!(db.list(&ListFilter::default()).unwrap().is_empty());
    }
}
