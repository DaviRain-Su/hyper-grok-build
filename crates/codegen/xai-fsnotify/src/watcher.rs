//! Filesystem notifications with debouncing and gitignore support.
//!
//! ## Ignoring Files
//!
//! Events for ignored files (`.gitignore` and custom patterns) are filtered out
//! before being sent to consumers.
//!
//! ## Watch Strategies
//!
//! Two OS-watch layouts, chosen per platform (see [`WatchStrategy`]):
//!
//! - **Fan-out** (macOS / Windows): root non-recursive + one *recursive* watch
//!   per non-ignored top-level child (capped, else recursive root). Recursion
//!   there is kernel-side (FSEvents / ReadDirectoryChangesW) — one handle per
//!   watch regardless of tree size — so skipping only *top-level* ignored dirs
//!   is enough.
//! - **Per-dir** (Linux): inotify has no kernel recursion; notify emulates it
//!   by walking the tree and adding **one watch descriptor per directory**,
//!   including gitignored trees (`node_modules/`, `target/`, `.venv/`) nested
//!   below the top level. That exhausts `fs.inotify.max_user_watches` for
//!   every process on the box. Instead we walk with the `ignore` crate
//!   (gitignore-aware at every depth), add a *non-recursive* watch per
//!   surviving dir — shallow-first, bounded by [`max_watch_budget`] — and
//!   maintain the set incrementally: new dirs are watched parent-before-listing
//!   with synthetic `Created` backfill for files that raced the watch, deleted
//!   dirs are pruned by prefix. `.git` is watched surgically (non-recursive
//!   `.git` + `refs`, recursive `refs/heads` + `refs/tags`) instead of
//!   recursively, so `objects/` and `modules/` (13k+ dirs on big repos) cost
//!   nothing. `GROK_FSNOTIFY_PER_DIR=1|0` overrides the platform default.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::RecursiveMode;
use notify::event::EventKind;
use notify_debouncer_full::{
    DebounceEventResult, DebouncedEvent, Debouncer, NoCache, new_debouncer_opt,
};
use tokio::sync::mpsc;

const DEBOUNCE_MS: u64 = 100;

use crate::event::FsEventKind;

/// Raw OS-level event from the debouncer. Internal; the semantic public
/// `FsEvent` enum lives in `event.rs`.
#[derive(Debug, Clone)]
pub(crate) struct RawFsEvent {
    pub paths: Vec<PathBuf>,
    pub kind: FsEventKind,
}

/// Map a notify `EventKind` to our public `FsEventKind`. Returns `None`
/// for events we don't surface (Access, Any, Other) — filtered before
/// reaching consumers so the public enum has no unobservable variants.
fn map_event_kind(kind: &EventKind) -> Option<FsEventKind> {
    use notify::event::ModifyKind;
    match kind {
        EventKind::Create(_) => Some(FsEventKind::Created),
        EventKind::Modify(ModifyKind::Name(_)) => Some(FsEventKind::Renamed),
        EventKind::Modify(_) => Some(FsEventKind::Modified),
        EventKind::Remove(_) => Some(FsEventKind::Removed),
        EventKind::Access(_) | EventKind::Any | EventKind::Other => None,
    }
}

/// Internal raw OS-watcher config. The user-facing version is `crate::source::FsConfig`.
/// `.git/` is always allowed through; the source classifies internally.
#[derive(Debug, Clone)]
pub(crate) struct FsNotifyConfig {
    pub debounce_ms: u64,
    pub ignore_patterns: Vec<String>,
}

impl Default for FsNotifyConfig {
    fn default() -> Self {
        Self {
            debounce_ms: DEBOUNCE_MS,
            ignore_patterns: vec![],
        }
    }
}

/// Permissive on purpose: lets `.git/index.lock` and `.git/gc.pid` through
/// to drive the lock state machine. `crate::paths::classify_git_path` keeps
/// them out of `GitMetaChanged`. Do not unify.
fn is_git_path_for_watcher(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.contains(".git/index")
        || s.contains(".git/HEAD")
        || s.contains(".git/FETCH_HEAD")
        || s.contains(".git/refs/")
        || s.contains(".git/packed-refs")
        || s.contains(".git/gc.pid")
}

/// Sapling analogue of [`is_git_path_for_watcher`]: lets **only** `.sl/wlock`
/// through. `.sl/dirstate` is intentionally not watched — it is read on demand,
/// because a read-only `sl status` rewrites dirstate without moving the parent,
/// so watching it would turn every status into a refresh storm. Forward-slash
/// only, like its git sibling.
fn is_sl_path_for_watcher(path: &Path) -> bool {
    path.to_string_lossy().contains(".sl/wlock")
}

/// True if `p`'s final component is exactly `name` (`.git`/`.sl`). Uses
/// `file_name` rather than `Path::ends_with` to dodge clippy's
/// `path_ends_with_ext` false positive on `.sl`.
fn dir_named(p: &Path, name: &str) -> bool {
    p.file_name().is_some_and(|n| n == name)
}

/// Whether Sapling (`.sl`) support is enabled (default on; `GROK_FSNOTIFY_SAPLING=0`
/// or `false` disables it). Resolved once per watcher in `FsEventSource::start_on`
/// and threaded down, so discovery, watching, and filtering can't disagree.
pub(crate) fn sapling_enabled() -> bool {
    !matches!(
        std::env::var("GROK_FSNOTIFY_SAPLING").ok().as_deref(),
        Some("0") | Some("false")
    )
}

/// How OS watches are laid out over the workspace. See the module docs for
/// the platform rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchStrategy {
    /// Root non-recursive + recursive top-level children (or recursive root
    /// past [`MAX_TOP_LEVEL_FANOUT`]). Kernel-recursive backends.
    Fanout,
    /// One non-recursive watch per non-ignored directory, full depth.
    /// Emulated-recursion backends (inotify), where this is strictly cheaper.
    PerDir,
}

/// Resolve the strategy: `GROK_FSNOTIFY_PER_DIR=1|true` forces per-dir,
/// `=0|false` forces fan-out, otherwise per-dir on Linux (inotify) and
/// fan-out elsewhere. Resolved once in [`start_with_timeout`] like the
/// Sapling switch, so selection and maintenance can't disagree.
pub(crate) fn watch_strategy() -> WatchStrategy {
    match std::env::var("GROK_FSNOTIFY_PER_DIR").ok().as_deref() {
        Some("1") | Some("true") => WatchStrategy::PerDir,
        Some("0") | Some("false") => WatchStrategy::Fanout,
        _ if cfg!(target_os = "linux") => WatchStrategy::PerDir,
        _ => WatchStrategy::Fanout,
    }
}

/// Per-dir mode's total watch budget (`GROK_FSNOTIFY_MAX_WATCHES` overrides).
///
/// Watches are added shallow-first, so hitting the budget sheds the *deepest*
/// directories; a warning is logged once. The default stays within a typical
/// `fs.inotify.max_user_watches` (65,536 on many distros) while leaving room
/// for other processes — the entire point of this mode is not to starve them.
const DEFAULT_MAX_WATCHES: usize = 49_152;

pub(crate) fn max_watch_budget() -> usize {
    std::env::var("GROK_FSNOTIFY_MAX_WATCHES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MAX_WATCHES)
}

#[derive(Default)]
struct GitignoreCache {
    cache: HashMap<PathBuf, (SystemTime, Gitignore)>,
}

impl GitignoreCache {
    /// Check if a path should be ignored.
    ///
    /// With `watch_vcs`, the metadata files that drive the lock state machine
    /// pass through: git's (`.git/index`, `.git/HEAD`, …) and, when `sapling`,
    /// `.sl/wlock`. Everything else under `.git`/`.sl` stays ignored.
    fn is_ignored(&mut self, path: &Path, watch_vcs: bool, sapling: bool) -> bool {
        let is_dir = path.is_dir();
        let mut current_dir = path.parent();
        while let Some(dir) = current_dir {
            if dir_named(dir, ".git") {
                if watch_vcs && is_git_path_for_watcher(path) {
                    return false;
                }
                return true;
            }
            if sapling && dir_named(dir, ".sl") {
                if watch_vcs && is_sl_path_for_watcher(path) {
                    return false;
                }
                return true;
            }

            let gitignore_path = dir.join(".gitignore");
            if let Ok(metadata) = gitignore_path.metadata()
                && let Ok(mtime) = metadata.modified()
            {
                let gitignore = self.get_or_load(&gitignore_path, dir, mtime);
                let m = gitignore.matched_path_or_any_parents(path, is_dir);
                if m.is_ignore() {
                    return true;
                }
                if m.is_whitelist() {
                    // A negation rule in this (deeper) .gitignore explicitly
                    // un-ignores the path. Shallower .gitignore files must not override.
                    return false;
                }
            }
            current_dir = dir.parent();
        }
        false
    }

    fn get_or_load(&mut self, gitignore_path: &Path, root: &Path, mtime: SystemTime) -> &Gitignore {
        let key = gitignore_path.to_path_buf();

        if let Some((cached_mtime, _)) = self.cache.get(&key)
            && *cached_mtime == mtime
        {
            return &self.cache[&key].1;
        }

        let mut builder = GitignoreBuilder::new(root);
        let _ = builder.add(gitignore_path);
        let gitignore = builder.build().unwrap_or_else(|_| Gitignore::empty());
        self.cache.insert(key.clone(), (mtime, gitignore));
        &self.cache[&key].1
    }
}

fn merge_events(events: impl IntoIterator<Item = DebouncedEvent>) -> Vec<RawFsEvent> {
    let mut by_path: HashMap<PathBuf, FsEventKind> = HashMap::new();
    // Rename events preserve original path ordering from the OS ([old, new]),
    // so they bypass the HashMap merge and are emitted directly.
    let mut rename_events: Vec<RawFsEvent> = Vec::new();

    for event in events.into_iter() {
        let Some(kind) = map_event_kind(&event.kind) else {
            continue;
        };

        if kind == FsEventKind::Renamed {
            rename_events.push(RawFsEvent {
                paths: event.event.paths,
                kind: FsEventKind::Renamed,
            });
            continue;
        }

        let paths = event.event.paths;
        for path in paths {
            by_path
                .entry(path)
                .and_modify(|existing| match (*existing, kind) {
                    (_, FsEventKind::Removed) => *existing = FsEventKind::Removed,
                    (FsEventKind::Created, FsEventKind::Modified) => {}
                    (FsEventKind::Modified, FsEventKind::Created) => {
                        *existing = FsEventKind::Created
                    }
                    _ => {}
                })
                .or_insert(kind);
        }
    }

    let mut result: HashMap<FsEventKind, Vec<PathBuf>> = HashMap::new();
    for (path, kind) in by_path {
        result.entry(kind).or_default().push(path);
    }

    let mut merged: Vec<RawFsEvent> = result
        .into_iter()
        .map(|(kind, paths)| RawFsEvent { paths, kind })
        .collect();
    merged.extend(rename_events);
    merged
}

/// Work forwarded from the debouncer callback (on notify's thread) to the
/// watcher thread that owns the debouncer.
enum WatchCommand {
    /// Re-evaluate the recursive child watches after a top-level structural change.
    /// Fan-out mode only.
    Reconcile,
    /// Per-dir mode: apply a structural delta. `pruned` (removed/renamed-away
    /// roots) is processed **before** `added` (created/renamed-in dirs) so a
    /// rename never `unwatch`es the watch descriptor its new path just re-bound
    /// (inotify wds follow inodes).
    Update {
        pruned: Vec<PathBuf>,
        added: Vec<PathBuf>,
    },
    /// Stop the watcher thread.
    Shutdown,
}

pub(crate) struct FsNotifyHandle {
    cmd_tx: Option<std::sync::mpsc::Sender<WatchCommand>>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Live OS-watch count (workspace + VCS), for stats/benchmarks.
    watch_count: Arc<AtomicUsize>,
}

impl FsNotifyHandle {
    /// Number of OS-level watches currently held (per-dir mode counts one per
    /// directory; fan-out mode one per `watch()` call).
    pub(crate) fn watch_count(&self) -> usize {
        self.watch_count.load(Ordering::Relaxed)
    }
}

impl Drop for FsNotifyHandle {
    fn drop(&mut self) {
        tracing::debug!("fs_notify: stopping watcher thread");
        // Explicit signal: the callback holds another sender, so dropping ours
        // wouldn't disconnect the thread's `recv`.
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.send(WatchCommand::Shutdown);
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

fn build_globsets(patterns: &[String]) -> (Option<GlobSet>, Option<GlobSet>) {
    let mut ignore_builder = GlobSetBuilder::new();
    let mut include_builder = GlobSetBuilder::new();

    for pattern in patterns {
        let (is_negation, raw_pattern) = pattern
            .strip_prefix('!')
            .map_or((false, pattern.as_str()), |p| (true, p));

        // Make patterns match anywhere in the path with **/ prefix if needed
        let glob_pattern = if raw_pattern.starts_with("**/") || raw_pattern.starts_with('/') {
            raw_pattern.to_string()
        } else {
            format!("**/{raw_pattern}")
        };

        if let Ok(glob) = Glob::new(&glob_pattern) {
            if is_negation {
                include_builder.add(glob);
            } else {
                ignore_builder.add(glob);
            }
        } else {
            tracing::warn!("invalid pattern: {}", pattern);
        }
    }

    let ignore_set = ignore_builder.build().ok().filter(|s| !s.is_empty());
    let include_set = include_builder.build().ok().filter(|s| !s.is_empty());

    (ignore_set, include_set)
}

/// Default timeout for watcher initialization.
const WATCHER_INIT_TIMEOUT_SECS: u64 = 10;

/// Fan out (watch each non-ignored top-level child recursively) only up to this
/// many children; above it, use one recursive root watch.
///
/// Gated on width, not on whether anything is ignored now: reconcile re-evaluates
/// the set on structural changes, so a `target/` created after start is still
/// excluded — a "fan out only if something is ignored" gate would miss it. The
/// cap bounds fan-out's one-`watch()`-per-child cost for wide trees with nothing
/// to skip, where a single recursive watch is cheaper.
const MAX_TOP_LEVEL_FANOUT: usize = 64;

#[derive(Debug, Clone)]
struct StartProgress {
    started_at: Instant,
    stage: &'static str,
    stage_started_at: Instant,
    timeline: Vec<(&'static str, Duration)>,
}

impl StartProgress {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            started_at: now,
            stage: "init",
            stage_started_at: now,
            timeline: vec![("init", Duration::from_millis(0))],
        }
    }

    fn set_stage(&mut self, stage: &'static str) {
        let t = self.started_at.elapsed();
        self.stage = stage;
        self.stage_started_at = Instant::now();
        // Keep this bounded to avoid unbounded growth.
        if self.timeline.len() < 32 {
            self.timeline.push((stage, t));
        }
    }

    fn snapshot(
        &self,
    ) -> (
        &'static str,
        Duration,
        Duration,
        Vec<(&'static str, Duration)>,
    ) {
        (
            self.stage,
            self.stage_started_at.elapsed(),
            self.started_at.elapsed(),
            self.timeline.clone(),
        )
    }
}

/// True if `path` is an immediate child of `root`.
fn is_top_level_child(path: &Path, root: &Path) -> bool {
    path.parent() == Some(root)
}

/// True if an event is a structural change (create/remove/rename) to a direct
/// child of `root` — the only kind that can add or remove a top-level watch.
fn event_triggers_reconcile(kind: FsEventKind, paths: &[PathBuf], root: &Path) -> bool {
    matches!(
        kind,
        FsEventKind::Created | FsEventKind::Removed | FsEventKind::Renamed
    ) && paths.iter().any(|p| is_top_level_child(p, root))
}

/// Per-dir mode: classify one event's paths into watch-set delta *candidates*.
///
/// Classified primarily by **on-disk state**, because backends report
/// structure ambiguously: FSEvents can coalesce a subtree removal into
/// `Modified` on the (now-vanished) parent, [`merge_events`] folds a
/// remove+recreate into `Removed`, and renames arrive as `From`/`To`/`Both`
/// shapes under `NoCache`. A path that is a directory right now (lstat;
/// symlinks excluded) is an add candidate; anything else (missing, file,
/// symlink) is a prune candidate.
///
/// The event *kind* contributes one thing state can't: a **structural** event
/// (create/remove/rename) on a still-existing dir may be a delete+recreate
/// inside one debounce window — the old inode's inotify watch is already dead
/// even though the path looks watched — so the dir is *also* pruned, forcing
/// an unwatch/re-watch re-arm (prunes are processed before adds).
///
/// Both lists are candidates only: the watcher thread rejects already-watched
/// adds and never-watched prunes in O(1), so the common case (file events,
/// dir-metadata touches) costs one `HashSet` lookup.
fn scan_per_dir_updates(
    kind: FsEventKind,
    paths: &[PathBuf],
    pruned: &mut Vec<PathBuf>,
    added: &mut Vec<PathBuf>,
) {
    let structural = matches!(
        kind,
        FsEventKind::Created | FsEventKind::Removed | FsEventKind::Renamed
    );
    for p in paths {
        let is_dir = p.symlink_metadata().is_ok_and(|m| m.file_type().is_dir());
        if is_dir {
            if structural {
                pruned.push(p.clone()); // Re-arm a possibly-dead watch.
            }
            added.push(p.clone());
        } else {
            pruned.push(p.clone());
        }
    }
}

/// Locate the `.git` directory governing `watch_path` (searching its ancestors).
///
/// A real (non-symlink) `.git` directory is returned directly via a cheap
/// `symlink_metadata` check (no link-follow), and lives inside the canonical
/// ancestor so it can't escape. A `.git` file or symlink is resolved through
/// `git2`, which rejects a pointer to a non-git target (e.g. a planted
/// `gitdir: ~/.ssh` or `ln -s ~/.ssh .git`) instead of watching it.
fn find_git_dir(watch_path: &Path) -> Option<PathBuf> {
    for ancestor in watch_path.ancestors() {
        let dot_git = ancestor.join(".git");
        let Ok(meta) = dot_git.symlink_metadata() else {
            continue;
        };
        if meta.file_type().is_dir() {
            return Some(dunce::canonicalize(&dot_git).unwrap_or(dot_git));
        }
        // A `.git` file or symlink: let git validate the target before watching.
        if let Ok(repo) = git2::Repository::open(ancestor) {
            let gd = repo.path().to_path_buf();
            return Some(dunce::canonicalize(&gd).unwrap_or(gd));
        }
    }
    None
}

/// Locate the `.sl` working-copy directory governing `watch_path` (ancestor
/// walk), mirroring [`find_git_dir`]'s real-directory branch: a non-symlink
/// `.sl` dir via `symlink_metadata` (no link-follow), canonicalized so it can't
/// escape. Sapling has no `.sl`-file indirection.
pub(crate) fn find_sl_dir(watch_path: &Path) -> Option<PathBuf> {
    for ancestor in watch_path.ancestors() {
        let dot_sl = ancestor.join(".sl");
        let Ok(meta) = dot_sl.symlink_metadata() else {
            continue;
        };
        if meta.file_type().is_dir() {
            return Some(dunce::canonicalize(&dot_sl).unwrap_or(dot_sl));
        }
    }
    None
}

/// Whether a discovered VCS metadata dir (`.git`/`.sl`) needs its own watch:
/// always in fan-out mode (the root is non-recursive); under a recursive root
/// only for an *external* (ancestor) dir — an internal one is already covered,
/// so re-watching it would be a redundant double-watch.
fn should_watch_separate_vcs_dir(fanout: bool, vcs_dir: &Path, watch_path: &Path) -> bool {
    fanout || !vcs_dir.starts_with(watch_path)
}

/// Apply the custom ignore/include globsets (a negation `include` wins). `None`
/// for both leaves the path unfiltered.
fn passes_custom_globs(
    path: &Path,
    custom_ignore: &Option<GlobSet>,
    custom_include: &Option<GlobSet>,
) -> bool {
    if let Some(include_set) = custom_include.as_ref()
        && include_set.is_match(path)
    {
        return true;
    }
    if let Some(ignore_set) = custom_ignore.as_ref()
        && ignore_set.is_match(path)
    {
        return false;
    }
    true
}

/// `WalkBuilder` configured for watch selection: honors `.gitignore`,
/// `.git/info/exclude`, global excludes, and `.ignore`, and never follows
/// symlinks (so watches can't leave the workspace via a symlinked dir).
fn ignore_walker(root: &Path, max_depth: Option<usize>) -> ignore::Walk {
    WalkBuilder::new(root)
        .hidden(false) // Let gitignore, not the leading dot, decide.
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .follow_links(false)
        .max_depth(max_depth)
        .build()
}

/// Immediate child directories of `root` to watch recursively.
///
/// The root is watched non-recursively (see [`start_with_timeout`]); each child
/// directory surviving gitignore + custom patterns is watched recursively, so a
/// gitignored top-level tree (e.g. `target/`) is never watched. `.git`/`.sl`
/// are excluded (watched separately); files are covered by the root watch;
/// symlinked children are skipped. A `custom_include` negation overrides
/// `custom_ignore` but not `.gitignore` (`WalkBuilder` never yields a gitignored
/// child).
fn select_top_level_watch_dirs(
    root: &Path,
    custom_ignore: &Option<GlobSet>,
    custom_include: &Option<GlobSet>,
) -> Vec<PathBuf> {
    // `usize::MAX` cap → never exceeded → always `Some`.
    select_top_level_watch_dirs_capped(root, custom_ignore, custom_include, usize::MAX)
        .unwrap_or_default()
}

/// Like [`select_top_level_watch_dirs`] but returns `None` once the non-ignored
/// count exceeds `max`, stopping the walk early.
///
/// The fan-out vs. recursive-root decision uses this non-ignored count
/// (gitignored children and `.git`/`.sl` don't count), and on `Some` the
/// returned list is reused as the initial watch set.
fn select_top_level_watch_dirs_capped(
    root: &Path,
    custom_ignore: &Option<GlobSet>,
    custom_include: &Option<GlobSet>,
    max: usize,
) -> Option<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for entry in ignore_walker(root, Some(1)).flatten() {
        if entry.depth() == 0 {
            continue; // `root` itself.
        }
        let path = entry.path();
        if !entry.file_type().is_some_and(|ft| ft.is_dir()) {
            continue;
        }
        // `.git`/`.sl` are watched separately (non-recursively for `.sl`),
        // never as recursive workspace children.
        if dir_named(path, ".git") || dir_named(path, ".sl") {
            continue;
        }
        if passes_custom_globs(path, custom_ignore, custom_include) {
            if dirs.len() == max {
                return None; // one past the cap
            }
            dirs.push(path.to_path_buf());
        }
    }
    Some(dirs)
}

/// Ignore-aware walker that also **prunes descent** into `.git`/`.sl` named
/// dirs and custom-ignored dirs (gitignore pruning comes from the `ignore`
/// crate itself). Used by per-dir selection and incremental subtree adds so
/// both apply identical semantics at every depth. Never follows symlinks.
fn pruning_walker(
    root: &Path,
    custom_ignore: &Option<GlobSet>,
    custom_include: &Option<GlobSet>,
) -> ignore::Walk {
    let mut walker = WalkBuilder::new(root);
    walker
        .hidden(false) // Let gitignore, not the leading dot, decide.
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .follow_links(false);
    let custom_ignore = custom_ignore.clone();
    let custom_include = custom_include.clone();
    walker.filter_entry(move |entry| {
        if !entry.file_type().is_some_and(|ft| ft.is_dir()) {
            return true; // Files pass here; callers filter them separately.
        }
        let path = entry.path();
        if dir_named(path, ".git") || dir_named(path, ".sl") {
            return false; // VCS metadata is watched separately (or not at all).
        }
        passes_custom_globs(path, &custom_ignore, &custom_include)
    });
    walker.build()
}

/// All non-ignored directories under `root` (root excluded), full depth, for
/// per-dir mode — one non-recursive watch each.
///
/// Selection semantics match [`select_top_level_watch_dirs`] but applied at
/// every depth via [`pruning_walker`]. Returned **shallow-first** (stable
/// within a depth), so a watch budget sheds the deepest directories, keeping
/// coverage near the root.
fn select_per_dir_watch_dirs(
    root: &Path,
    custom_ignore: &Option<GlobSet>,
    custom_include: &Option<GlobSet>,
) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = pruning_walker(root, custom_ignore, custom_include)
        .flatten()
        .filter(|e| e.depth() > 0 && e.file_type().is_some_and(|ft| ft.is_dir()))
        .map(|e| e.into_path())
        .collect();
    // Walk order is DFS; re-order shallow-first so a budget cut is depth-based.
    dirs.sort_by_key(|d| d.components().count());
    dirs
}

/// The watches a discovered `.git` dir needs in per-dir mode, replacing the
/// fan-out mode's single recursive watch. Recursive `.git` is catastrophic on
/// inotify — `objects/` (256-way fan-out) and `modules/` (submodule git dirs)
/// are thousands of directories that the event filter would discard anyway.
///
/// Everything [`is_git_path_for_watcher`] passes is covered: `index`, `HEAD`,
/// `FETCH_HEAD`, `packed-refs`, `gc.pid` are direct children (non-recursive
/// `.git` watch); `refs/heads/**` + `refs/tags/**` recursive for branch/tag
/// moves. `refs` itself is non-recursive: `refs/remotes/**` (thousands of dirs
/// on fetch-heavy clones) is deliberately unwatched — remote updates still
/// surface via `FETCH_HEAD` and `packed-refs`. Worktree git dirs
/// (`.git/worktrees/<n>`) have no `refs/`, so they get just the non-recursive
/// watch, covering their `HEAD`/`index`.
fn per_dir_git_watches(git_dir: &Path) -> Vec<(PathBuf, RecursiveMode)> {
    let mut watches = vec![(git_dir.to_path_buf(), RecursiveMode::NonRecursive)];
    let refs = git_dir.join("refs");
    if refs.is_dir() {
        watches.push((refs.clone(), RecursiveMode::NonRecursive));
        for sub in ["heads", "tags"] {
            let p = refs.join(sub);
            if p.is_dir() {
                watches.push((p, RecursiveMode::Recursive));
            }
        }
    }
    watches
}

/// Pure set difference for watch reconciliation: dirs to add (desired but not
/// live) and to remove (live but no longer desired).
fn diff_watches(
    desired: &HashSet<PathBuf>,
    live: &HashSet<PathBuf>,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let to_add = desired.difference(live).cloned().collect();
    let to_remove = live.difference(desired).cloned().collect();
    (to_add, to_remove)
}

/// Reconcile the recursive child watches against the current on-disk children,
/// reusing [`select_top_level_watch_dirs`] so the runtime decision matches
/// startup. Newly-appeared non-ignored dirs are watched; deleted, renamed-away,
/// or now-ignored dirs are unwatched (so a later recreate re-arms and the set
/// stays bounded).
///
/// Only structural events trigger this, not ignore-rule edits, and it runs after
/// the debounce window — so a freshly-ignored dir stays watched (still
/// event-filtered) and a brand-new dir's pre-watch files aren't backfilled.
fn reconcile_top_level_watches(
    debouncer: &mut Debouncer<notify::RecommendedWatcher, NoCache>,
    watched: &mut HashSet<PathBuf>,
    root: &Path,
    custom_ignore: &Option<GlobSet>,
    custom_include: &Option<GlobSet>,
) {
    let desired: HashSet<PathBuf> =
        select_top_level_watch_dirs(root, custom_ignore, custom_include)
            .into_iter()
            .collect();

    let (to_add, to_remove) = diff_watches(&desired, watched);

    for dir in to_add {
        match debouncer.watch(&dir, RecursiveMode::Recursive) {
            Ok(()) => {
                watched.insert(dir);
            }
            Err(e) => tracing::warn!("failed to watch {:?}: {:?}", dir, e),
        }
    }
    for dir in to_remove {
        // The OS often drops a deleted dir's watch already, so unwatch errors
        // are expected and ignored.
        let _ = debouncer.unwatch(&dir);
        watched.remove(&dir);
    }
}

/// Synthetic-`Created` backfill batch size: bounds per-event path counts for
/// consumers when a huge tree appears at once (vendored checkout, `tar -x`).
const BACKFILL_BATCH: usize = 512;

/// Per-dir watches armed per command-loop iteration. Each `watch()` is a
/// round-trip into notify's event-loop thread (~100–300µs), so the chunk
/// bounds command-handling latency during startup arming (~0.1s per chunk)
/// while a monorepo-scale backlog (50k+ dirs) still arms in seconds.
const ARM_CHUNK: usize = 512;

/// Largest pending backlog that per-dir startup arms *before* signaling
/// ready. Each `watch()` costs a ~100–300µs round-trip into notify's event
/// loop, so this bound keeps the added ready-latency under ~1s while giving
/// typical repos (≤4k non-ignored dirs) a fully-armed watcher with **no
/// startup blind window**. Bigger selections signal ready immediately after
/// the root + top-level + VCS watches and arm the rest in background chunks
/// (shallow-first): events under a not-yet-armed deep dir can be missed until
/// its watch lands, which consumers doing an initial scan (indexer, hunk
/// tracker) absorb by construction — the same window the old fan-out code had
/// while notify walked each top-level subtree.
const ARM_SYNC_MAX: usize = 4096;

/// Arm up to [`ARM_CHUNK`] pending per-dir watches. An entry that vanished (or
/// was pruned) since selection fails not-found — expected, logged at debug. An
/// entry re-added by an `Update` in the meantime is skipped by the `contains`
/// check.
fn arm_pending_chunk(
    debouncer: &mut Debouncer<notify::RecommendedWatcher, NoCache>,
    watched: &mut HashSet<PathBuf>,
    pending: &mut std::collections::VecDeque<PathBuf>,
    mode: RecursiveMode,
) {
    for _ in 0..ARM_CHUNK {
        let Some(dir) = pending.pop_front() else {
            break;
        };
        if watched.contains(&dir) {
            continue;
        }
        match debouncer.watch(&dir, mode) {
            Ok(()) => {
                watched.insert(dir);
            }
            Err(e) => tracing::debug!("failed to arm pending watch {:?}: {:?}", dir, e),
        }
    }
    if pending.is_empty() {
        tracing::debug!(
            "fs_notify: watch arming complete ({} workspace dirs watched)",
            watched.len()
        );
    }
}

/// Per-dir mode: watch a newly created directory subtree.
///
/// Walk order closes the race with in-flight writers: each dir is watched
/// **when yielded, before its listing is read** (pre-order), so a file landing
/// after the listing is caught by the just-armed watch, and one landing before
/// is caught by the backfill — files can be reported twice (consumers are
/// idempotent) but never lost. Files present at walk time are emitted as
/// synthetic `Created` events (batched): they arrived while the dir was
/// unwatched, since the AddDirs command itself rode a ≥1-debounce-window delay
/// behind the `mkdir`.
///
/// Budget-aware: stops adding once `budget` is reached (warned upstream).
fn add_subtree_watches(
    debouncer: &mut Debouncer<notify::RecommendedWatcher, NoCache>,
    watched: &mut HashSet<PathBuf>,
    subtree_root: &Path,
    custom_ignore: &Option<GlobSet>,
    custom_include: &Option<GlobSet>,
    budget: usize,
    tx: &mpsc::UnboundedSender<RawFsEvent>,
) {
    // Symlink or vanished-since-event: nothing to watch.
    if !subtree_root
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_dir())
    {
        return;
    }
    // Already-watched roots are the common case (every event *on* a watched
    // dir makes it an add candidate): a fresh subtree starts unwatched, and
    // events *inside* a watched-but-stale subtree re-candidate their parent
    // anyway, so skipping here never strands a genuinely new dir.
    if watched.contains(subtree_root) {
        return;
    }
    let mut backfill: Vec<PathBuf> = Vec::new();
    let flush = |paths: &mut Vec<PathBuf>| {
        if !paths.is_empty() {
            let _ = tx.send(RawFsEvent {
                paths: std::mem::take(paths),
                kind: FsEventKind::Created,
            });
        }
    };

    for entry in pruning_walker(subtree_root, custom_ignore, custom_include).flatten() {
        let path = entry.path();
        if entry.file_type().is_some_and(|ft| ft.is_dir()) {
            if watched.contains(path) {
                continue;
            }
            if watched.len() >= budget {
                tracing::warn!(
                    "fs_notify: watch budget ({budget}) reached while adding {:?}; deeper dirs unwatched",
                    subtree_root
                );
                break;
            }
            match debouncer.watch(path, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    watched.insert(path.to_path_buf());
                }
                Err(e) => tracing::warn!("failed to watch new dir {:?}: {:?}", path, e),
            }
        } else if entry.depth() > 0 && passes_custom_globs(path, custom_ignore, custom_include) {
            backfill.push(path.to_path_buf());
            if backfill.len() >= BACKFILL_BATCH {
                flush(&mut backfill);
            }
        }
    }
    flush(&mut backfill);
}

/// Per-dir mode: drop bookkeeping (and best-effort OS watches) for a removed
/// or renamed-away directory subtree. The kernel already dropped watches on
/// deleted dirs (`IN_IGNORED`), but the explicit unwatch keeps notify's
/// path-keyed bookkeeping clean and — crucially for renames — frees the watch
/// descriptor *before* the destination path is re-watched (see
/// [`WatchCommand::Update`] ordering).
fn prune_subtree_watches(
    debouncer: &mut Debouncer<notify::RecommendedWatcher, NoCache>,
    watched: &mut HashSet<PathBuf>,
    subtree_root: &Path,
) {
    let stale: Vec<PathBuf> = watched
        .iter()
        .filter(|p| p.starts_with(subtree_root))
        .cloned()
        .collect();
    for dir in stale {
        let _ = debouncer.unwatch(&dir); // Usually already gone; errors expected.
        watched.remove(&dir);
    }
}

pub(crate) fn start(
    watch_path: PathBuf,
    config: FsNotifyConfig,
    sapling: bool,
) -> Result<(mpsc::UnboundedReceiver<RawFsEvent>, FsNotifyHandle), crate::FsNotifyError> {
    start_with_timeout(
        watch_path,
        config,
        sapling,
        watch_strategy(),
        Duration::from_secs(WATCHER_INIT_TIMEOUT_SECS),
    )
}

/// Start with a custom timeout and explicit strategy (tests pass these
/// directly to avoid process-global env races). `sapling` is the resolved
/// kill-switch, threaded from `FsEventSource::start_on`.
pub(crate) fn start_with_timeout(
    watch_path: PathBuf,
    config: FsNotifyConfig,
    sapling: bool,
    strategy: WatchStrategy,
    init_timeout: Duration,
) -> Result<(mpsc::UnboundedReceiver<RawFsEvent>, FsNotifyHandle), crate::FsNotifyError> {
    let progress = Arc::new(Mutex::new(StartProgress::new()));
    let (tx, rx) = mpsc::unbounded_channel();
    let debounce_duration = Duration::from_millis(config.debounce_ms);
    // `.git/` (and, when `sapling`, `.sl/wlock`) pass through; the source
    // classifies internally.
    let watch_vcs = true;
    let (custom_ignore, custom_include) = build_globsets(&config.ignore_patterns);
    // Arc so the watcher thread and its debouncer callback share, not copy, them.
    let custom_ignore = Arc::new(custom_ignore);
    let custom_include = Arc::new(custom_include);

    // Canonicalize once: notify echoes event paths under the watched path, but
    // macOS FSEvents resolves symlinks, so a raw (symlinked/relative) root would
    // never match `parent() == root` and dynamic watching would silently break.
    let watch_path = dunce::canonicalize(&watch_path).unwrap_or(watch_path);

    tracing::debug!("fs_notify: starting watcher under {:?}", watch_path);

    // Channel to signal when watcher is ready
    let (ready_tx, ready_rx) =
        std::sync::mpsc::channel::<Result<(), Box<dyn std::error::Error + Send + Sync>>>();

    // Carries reconcile requests from the debouncer callback to the owning
    // thread, plus the shutdown signal from `FsNotifyHandle`.
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<WatchCommand>();
    let cmd_tx_cb = cmd_tx.clone();
    let cmd_tx_for_handle = cmd_tx;

    // Live OS-watch count, shared with the handle for stats/benchmarks.
    let watch_count = Arc::new(AtomicUsize::new(0));
    let watch_count_thread = Arc::clone(&watch_count);

    // Synthetic-backfill sender for per-dir subtree adds (the debouncer
    // callback owns the primary sender).
    let backfill_tx = tx.clone();

    if let Ok(mut p) = progress.lock() {
        p.set_stage("spawning_watcher_thread");
    }

    let progress_for_thread = progress.clone();

    let watcher_loop = move || {
        let update_stage = |stage: &'static str| {
            if let Ok(mut p) = progress_for_thread.lock() {
                p.set_stage(stage);
            }
        };

        update_stage("watcher_thread_started");

        let mut gitignore_cache = GitignoreCache::default();
        let watch_path_cb = watch_path.clone();
        let custom_ignore_cb = Arc::clone(&custom_ignore);
        let custom_include_cb = Arc::clone(&custom_include);

        // Use NoCache to avoid walking the entire directory tree for file ID tracking.
        // This prevents multi-GB memory usage on large repos. Trade-off: rename events
        // may appear as Remove+Create pairs instead of a single Rename event.
        update_stage("creating_debouncer");
        let debouncer_result = new_debouncer_opt::<_, notify::RecommendedWatcher, _>(
            debounce_duration,
            None,
            move |result: DebounceEventResult| match result {
                Ok(events) => {
                    // Per-path (not per-event) so gitignored paths can't leak via
                    // multi-path debounced events.
                    let mut needs_reconcile = false;
                    let mut pruned: Vec<PathBuf> = Vec::new();
                    let mut added: Vec<PathBuf> = Vec::new();
                    for mut event in merge_events(events) {
                        event.paths.retain(|path| {
                            if let Some(ref include_set) = *custom_include_cb
                                && include_set.is_match(path)
                            {
                                return true;
                            }
                            if gitignore_cache.is_ignored(path, watch_vcs, sapling) {
                                return false;
                            }
                            if let Some(ref ignore_set) = *custom_ignore_cb
                                && ignore_set.is_match(path)
                            {
                                return false;
                            }
                            true
                        });
                        if event.paths.is_empty() {
                            continue;
                        }
                        // Post-retain, so ignored paths never grow the watch set.
                        match strategy {
                            WatchStrategy::Fanout => {
                                if event_triggers_reconcile(
                                    event.kind,
                                    &event.paths,
                                    &watch_path_cb,
                                ) {
                                    needs_reconcile = true;
                                }
                            }
                            WatchStrategy::PerDir => scan_per_dir_updates(
                                event.kind,
                                &event.paths,
                                &mut pruned,
                                &mut added,
                            ),
                        }
                        let _ = tx.send(event);
                    }
                    // One command per batch, not per event, to coalesce bursts.
                    if needs_reconcile {
                        let _ = cmd_tx_cb.send(WatchCommand::Reconcile);
                    }
                    if !pruned.is_empty() || !added.is_empty() {
                        let _ = cmd_tx_cb.send(WatchCommand::Update { pruned, added });
                    }
                }
                Err(errors) => {
                    for e in errors {
                        tracing::warn!("fs_notify error: {:?}", e);
                    }
                }
            },
            NoCache,
            // Don't follow symlinks: watches can't leave the workspace, and (a
            // behavior change) in-workspace symlinked dirs aren't traversed, so
            // file events under them no longer surface.
            notify::Config::default().with_follow_symlinks(false),
        );

        match debouncer_result {
            Ok(mut debouncer) => {
                update_stage("adding_watches");

                let per_dir = strategy == WatchStrategy::PerDir;
                let budget = max_watch_budget();

                // Initial layout per strategy (see module docs). `initial` is
                // `Some(dirs)` whenever the root watch is non-recursive.
                let initial = match strategy {
                    WatchStrategy::PerDir => {
                        let mut dirs =
                            select_per_dir_watch_dirs(&watch_path, &custom_ignore, &custom_include);
                        if dirs.len() > budget {
                            tracing::warn!(
                                "fs_notify: {} non-ignored dirs exceed watch budget {budget}; \
                                 shedding the deepest (raise with GROK_FSNOTIFY_MAX_WATCHES)",
                                dirs.len()
                            );
                            dirs.truncate(budget);
                        }
                        Some(dirs)
                    }
                    // Fan-out vs. recursive-root decision on the non-ignored
                    // count; stops early past the cap (see `MAX_TOP_LEVEL_FANOUT`).
                    WatchStrategy::Fanout => select_top_level_watch_dirs_capped(
                        &watch_path,
                        &custom_ignore,
                        &custom_include,
                        MAX_TOP_LEVEL_FANOUT,
                    ),
                };
                let root_non_recursive = initial.is_some();
                let root_mode = if root_non_recursive {
                    // Non-recursive: catches top-level files and the
                    // creation/removal of top-level directories.
                    RecursiveMode::NonRecursive
                } else {
                    RecursiveMode::Recursive
                };
                // Per-dir children are leaves of the layout; fan-out children
                // are kernel-recursive subtrees.
                let child_mode = if per_dir {
                    RecursiveMode::NonRecursive
                } else {
                    RecursiveMode::Recursive
                };
                if let Err(e) = debouncer.watch(&watch_path, root_mode) {
                    tracing::error!("failed to watch root: {:?}", e);
                    let _ = ready_tx.send(Err(Box::new(e)));
                    return;
                }

                // Reuse the already-computed selection as the initial watch
                // set. Fan-out installs everything synchronously (≤64+1
                // `watch()` calls). Per-dir installs only the root's immediate
                // children synchronously — each `watch()` is a round-trip into
                // notify's event-loop thread, and a monorepo-scale selection
                // (tens of thousands of dirs) would block readiness past the
                // init timeout — the rest arms in the command loop in
                // shallow-first chunks (see `ARM_CHUNK`) right after `ready`.
                let mut watched_dirs: HashSet<PathBuf> = HashSet::new();
                let mut pending_dirs: std::collections::VecDeque<PathBuf> =
                    std::collections::VecDeque::new();
                if let Some(dirs) = initial {
                    let sync_head: Vec<PathBuf> = if per_dir {
                        let (head, tail): (Vec<PathBuf>, Vec<PathBuf>) = dirs
                            .into_iter()
                            .partition(|d| d.parent() == Some(watch_path.as_path()));
                        pending_dirs = tail.into(); // Still shallow-first.
                        head
                    } else {
                        dirs
                    };
                    for dir in sync_head {
                        match debouncer.watch(&dir, child_mode) {
                            Ok(()) => {
                                watched_dirs.insert(dir);
                            }
                            Err(e) => tracing::warn!("failed to watch {:?}: {:?}", dir, e),
                        }
                    }
                }

                // Watch `.git` for HEAD/index/lock events; it is excluded from
                // the workspace children. Fan-out: one recursive watch (cheap
                // on kernel-recursive backends). Per-dir: surgical watches —
                // recursive `.git` costs one inotify watch per dir under it
                // (`objects/` + `modules/` = 13k+ dirs on submodule-heavy
                // repos) for events the filter would drop anyway.
                let mut vcs_watches = 0usize;
                // Retained beyond the watch setup: the per-dir Update arm
                // excludes paths under these from workspace watch management
                // (their events pass the VCS filter, but their dirs belong to
                // the surgical VCS watches, never `watched_dirs`).
                let git_dir = if watch_vcs {
                    find_git_dir(&watch_path)
                } else {
                    None
                };
                let sl_dir = if watch_vcs && sapling {
                    find_sl_dir(&watch_path)
                } else {
                    None
                };
                if let Some(gd) = git_dir
                    .as_deref()
                    .filter(|gd| should_watch_separate_vcs_dir(root_non_recursive, gd, &watch_path))
                {
                    let git_watches = if per_dir {
                        per_dir_git_watches(gd)
                    } else {
                        vec![(gd.to_path_buf(), RecursiveMode::Recursive)]
                    };
                    for (p, mode) in git_watches {
                        if let Err(e) = debouncer.watch(&p, mode) {
                            tracing::warn!("failed to watch git path {:?}: {:?}", p, e);
                        } else {
                            vcs_watches += 1;
                        }
                    }
                    tracing::debug!("fs_notify: watching git dir {:?}", gd);
                }

                // Watch `.sl` non-recursively: its sole whitelisted marker
                // (`wlock`) is a direct child, so `.sl/store` is never walked.
                if let Some(sd) = sl_dir
                    .as_deref()
                    .filter(|sd| should_watch_separate_vcs_dir(root_non_recursive, sd, &watch_path))
                {
                    if let Err(e) = debouncer.watch(sd, RecursiveMode::NonRecursive) {
                        tracing::warn!("failed to watch sl dir {:?}: {:?}", sd, e);
                    } else {
                        vcs_watches += 1;
                        tracing::debug!("fs_notify: watching sl dir {:?}", sd);
                    }
                }

                // Small backlogs are armed fully before declaring readiness
                // (no blind window); monorepo-scale ones continue in the
                // command loop below (see `ARM_SYNC_MAX`).
                if pending_dirs.len() <= ARM_SYNC_MAX {
                    while !pending_dirs.is_empty() {
                        arm_pending_chunk(
                            &mut debouncer,
                            &mut watched_dirs,
                            &mut pending_dirs,
                            child_mode,
                        );
                    }
                }

                watch_count_thread.store(1 + watched_dirs.len() + vcs_watches, Ordering::Relaxed);
                tracing::debug!(
                    "fs_notify started: watching {:?} (strategy={:?}, {} workspace dirs armed + {} pending + {} vcs watches, {}ms debounce)",
                    watch_path,
                    strategy,
                    watched_dirs.len(),
                    pending_dirs.len(),
                    vcs_watches,
                    debounce_duration.as_millis()
                );

                // Signal ready once the root, its immediate children, the VCS
                // watches, and everything the grace period covered are
                // established (any per-dir remainder arms in chunks below;
                // fan-out never has anything pending).
                update_stage("signaling_ready");
                let _ = ready_tx.send(Ok(()));

                update_stage("running");

                // Handle one non-shutdown command. Returns `false` on shutdown.
                let handle_command =
                    |cmd: WatchCommand,
                     debouncer: &mut Debouncer<notify::RecommendedWatcher, NoCache>,
                     watched_dirs: &mut HashSet<PathBuf>|
                     -> bool {
                        match cmd {
                            WatchCommand::Reconcile => {
                                // Fan-out only (the callback never sends it in
                                // per-dir mode; guard anyway).
                                if !per_dir && root_non_recursive {
                                    reconcile_top_level_watches(
                                        debouncer,
                                        watched_dirs,
                                        &watch_path,
                                        &custom_ignore,
                                        &custom_include,
                                    );
                                }
                            }
                            WatchCommand::Update { pruned, added } => {
                                if per_dir {
                                    // Prune before add (rename wd re-binding; see
                                    // `WatchCommand::Update`). The `contains` gate
                                    // makes removed *files* O(1); a watched dir
                                    // whose ancestor was shed by the budget can
                                    // strand descendants in bookkeeping — the
                                    // kernel already dropped those watches, and a
                                    // recreate re-arms via `add_subtree_watches`'
                                    // own `contains` check.
                                    for p in &pruned {
                                        if watched_dirs.contains(p) {
                                            prune_subtree_watches(debouncer, watched_dirs, p);
                                        }
                                    }
                                    for a in &added {
                                        // The root is watched separately and always
                                        // live; without this guard a metadata touch
                                        // on the root (`chmod`/`touch`) would walk
                                        // the whole workspace as an "add". Dirs
                                        // under `.git`/`.sl` (whose *events* pass
                                        // the VCS filter, e.g. a new `refs/heads/
                                        // feature/` namespace) belong to the
                                        // surgical VCS watches, not the workspace
                                        // set.
                                        if *a == watch_path
                                            || git_dir.as_deref().is_some_and(|d| a.starts_with(d))
                                            || sl_dir.as_deref().is_some_and(|d| a.starts_with(d))
                                        {
                                            continue;
                                        }
                                        add_subtree_watches(
                                            debouncer,
                                            watched_dirs,
                                            a,
                                            &custom_ignore,
                                            &custom_include,
                                            budget,
                                            &backfill_tx,
                                        );
                                    }
                                }
                            }
                            WatchCommand::Shutdown => return false,
                        }
                        true
                    };

                // Own the debouncer; alternate between draining commands and
                // arming pending per-dir watches in shallow-first chunks, so a
                // huge initial selection can't starve prune/add commands (or
                // shutdown). Once `pending_dirs` empties this degrades to a
                // plain blocking `recv` loop.
                'run: loop {
                    // Drain whatever is queued without blocking.
                    loop {
                        match cmd_rx.try_recv() {
                            Ok(cmd) => {
                                if !handle_command(cmd, &mut debouncer, &mut watched_dirs) {
                                    break 'run;
                                }
                            }
                            Err(std::sync::mpsc::TryRecvError::Empty) => break,
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => break 'run,
                        }
                    }

                    if pending_dirs.is_empty() {
                        watch_count_thread
                            .store(1 + watched_dirs.len() + vcs_watches, Ordering::Relaxed);
                        // Nothing to arm: block until the next command.
                        match cmd_rx.recv() {
                            Ok(cmd) => {
                                if !handle_command(cmd, &mut debouncer, &mut watched_dirs) {
                                    break 'run;
                                }
                            }
                            Err(_) => break 'run,
                        }
                    } else {
                        arm_pending_chunk(
                            &mut debouncer,
                            &mut watched_dirs,
                            &mut pending_dirs,
                            child_mode,
                        );
                        watch_count_thread
                            .store(1 + watched_dirs.len() + vcs_watches, Ordering::Relaxed);
                    }
                }
                watch_count_thread.store(1 + watched_dirs.len() + vcs_watches, Ordering::Relaxed);
                tracing::debug!("fs_notify stopped");
            }
            Err(e) => {
                tracing::error!("failed to create debouncer: {:?}", e);
                let _ = ready_tx.send(Err(Box::new(e)));
            }
        }
    };
    let thread = std::thread::Builder::new()
        .name("fsnotify-watcher".into())
        .spawn(watcher_loop)
        .map_err(|e| crate::FsNotifyError::WatcherStart(Box::new(e)))?;

    // Wait for watcher to be ready (with timeout)
    if let Ok(mut p) = progress.lock() {
        p.set_stage("waiting_for_ready");
    }
    match ready_rx.recv_timeout(init_timeout) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(crate::FsNotifyError::WatcherStart(e)),
        Err(_) => {
            let (stage, stage_elapsed, total_elapsed, timeline) =
                progress.lock().map(|p| p.snapshot()).unwrap_or((
                    "unknown",
                    Duration::from_secs(0),
                    Duration::from_secs(0),
                    Vec::new(),
                ));
            tracing::debug!(
                "watcher start timed out ({}s): stage={}, stage_elapsed={:?}, total_elapsed={:?}, timeline={:?}",
                init_timeout.as_secs(),
                stage,
                stage_elapsed,
                total_elapsed,
                timeline
            );
            // No `FsNotifyHandle` owns the thread on this path, so queue a
            // Shutdown: when the slow setup finishes and the thread reaches its
            // recv loop, it self-terminates and releases its watches instead of
            // leaking (the callback holds the other sender, so it never
            // disconnects on its own).
            let _ = cmd_tx_for_handle.send(WatchCommand::Shutdown);
            return Err(crate::FsNotifyError::Timeout);
        }
    }

    Ok((
        rx,
        FsNotifyHandle {
            cmd_tx: Some(cmd_tx_for_handle),
            thread: Some(thread),
            watch_count,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use xai_tracing_macros::teprintln;

    #[test]
    fn test_map_event_kind() {
        use notify::event::{CreateKind, ModifyKind, RemoveKind};

        assert_eq!(
            map_event_kind(&EventKind::Create(CreateKind::File)),
            Some(FsEventKind::Created)
        );
        assert_eq!(
            map_event_kind(&EventKind::Modify(ModifyKind::Data(
                notify::event::DataChange::Content
            ))),
            Some(FsEventKind::Modified)
        );
        assert_eq!(
            map_event_kind(&EventKind::Modify(ModifyKind::Name(
                notify::event::RenameMode::Both
            ))),
            Some(FsEventKind::Renamed)
        );
        assert_eq!(
            map_event_kind(&EventKind::Remove(RemoveKind::File)),
            Some(FsEventKind::Removed)
        );
        assert_eq!(map_event_kind(&EventKind::Other), None);
    }

    // ========================================================================
    // Integration tests with real filesystem and debouncer
    // These tests are serialized because macOS FSEvents has limited resources
    // when many watchers are created simultaneously.
    // ========================================================================

    mod integration {
        use super::*;
        use serial_test::serial;
        use std::fs;
        use std::time::Duration;
        use tempfile::TempDir;

        /// Default debounce time for tests
        const TEST_DEBOUNCE_MS: u64 = 50;
        /// Max time to wait for events after debounce (debounce + buffer)
        const EVENT_WAIT_MS: u64 = 300;
        /// Timeout for watcher initialization in tests
        const TEST_INIT_TIMEOUT: Duration = Duration::from_secs(15);
        /// Number of retries for starting watcher (helps with flaky FSEvents)
        const START_RETRIES: usize = 3;

        /// Start a watcher with retry logic for flaky FSEvents.
        fn start_with_retry(
            watch_path: PathBuf,
            config: FsNotifyConfig,
        ) -> Result<
            (
                tokio::sync::mpsc::UnboundedReceiver<RawFsEvent>,
                FsNotifyHandle,
            ),
            crate::FsNotifyError,
        > {
            start_with_retry_strategy(watch_path, config, watch_strategy())
        }

        /// Like [`start_with_retry`] but with an explicit strategy, so tests
        /// can exercise both layouts without process-global env races.
        fn start_with_retry_strategy(
            watch_path: PathBuf,
            config: FsNotifyConfig,
            strategy: WatchStrategy,
        ) -> Result<
            (
                tokio::sync::mpsc::UnboundedReceiver<RawFsEvent>,
                FsNotifyHandle,
            ),
            crate::FsNotifyError,
        > {
            let mut last_error = None;
            for attempt in 1..=START_RETRIES {
                match start_with_timeout(
                    watch_path.clone(),
                    config.clone(),
                    true,
                    strategy,
                    TEST_INIT_TIMEOUT,
                ) {
                    Ok(result) => return Ok(result),
                    Err(e) => {
                        teprintln!(
                            "Watcher start attempt {}/{} failed: {}",
                            attempt,
                            START_RETRIES,
                            e
                        );
                        last_error = Some(e);
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
            Err(last_error.unwrap_or_else(|| {
                crate::FsNotifyError::WatcherStart(
                    std::io::Error::other("failed to start watcher").into(),
                )
            }))
        }

        /// Helper to collect events with timeout, with early exit once we get events
        /// and a quiet period passes without new ones.
        fn collect_events_smart(
            rx: &mut tokio::sync::mpsc::UnboundedReceiver<RawFsEvent>,
            max_wait: Duration,
            quiet_period: Duration,
        ) -> Vec<RawFsEvent> {
            let mut events = Vec::new();
            let deadline = std::time::Instant::now() + max_wait;
            let mut last_event_time = std::time::Instant::now();

            while std::time::Instant::now() < deadline {
                match rx.try_recv() {
                    Ok(event) => {
                        events.push(event);
                        last_event_time = std::time::Instant::now();
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                        // If we have events and quiet period elapsed, return early
                        if !events.is_empty() && last_event_time.elapsed() >= quiet_period {
                            return events;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                }
            }
            events
        }

        /// Collect events with default timing
        fn collect_events(
            rx: &mut tokio::sync::mpsc::UnboundedReceiver<RawFsEvent>,
        ) -> Vec<RawFsEvent> {
            collect_events_smart(
                rx,
                Duration::from_millis(EVENT_WAIT_MS),
                Duration::from_millis(50),
            )
        }

        #[test]
        #[serial]
        #[ignore = "flaky in CI — fs events not reliably delivered"]
        fn test_debouncer_create_file() {
            let temp_dir = TempDir::new().unwrap();
            let watch_path = dunce::canonicalize(temp_dir.path()).unwrap();

            let config = FsNotifyConfig {
                debounce_ms: TEST_DEBOUNCE_MS,
                ignore_patterns: vec![],
            };

            let (mut rx, _handle) = start_with_retry(watch_path.clone(), config).unwrap();

            // Create a file
            let test_file = watch_path.join("test.txt");
            fs::write(&test_file, "hello").unwrap();

            let events = collect_events(&mut rx);

            // Should have at least one Create event
            let create_events: Vec<_> = events
                .iter()
                .filter(|e| e.kind == FsEventKind::Created)
                .collect();
            assert!(
                !create_events.is_empty(),
                "Expected Create event, got: {:?}",
                events
            );

            // The path should contain our file
            let has_test_file = create_events
                .iter()
                .any(|e| e.paths.iter().any(|p| p.ends_with("test.txt")));
            assert!(has_test_file, "Create event should contain test.txt");
        }

        #[test]
        #[serial]
        fn test_debouncer_modify_file() {
            let temp_dir = TempDir::new().unwrap();
            let watch_path = dunce::canonicalize(temp_dir.path()).unwrap();

            // Create file before starting watcher
            let test_file = watch_path.join("existing.txt");
            fs::write(&test_file, "initial").unwrap();

            // Small delay to ensure file is stable before watcher starts
            std::thread::sleep(Duration::from_millis(50));

            let config = FsNotifyConfig {
                debounce_ms: TEST_DEBOUNCE_MS,
                ignore_patterns: vec![],
            };

            let (mut rx, _handle) = start_with_retry(watch_path.clone(), config).unwrap();

            // Modify the file
            fs::write(&test_file, "modified content").unwrap();

            let events = collect_events(&mut rx);

            // Should have an event for the file (Modify on most platforms,
            // but macOS FSEvents may report Create in some cases)
            let has_file_event = events.iter().any(|e| {
                (e.kind == FsEventKind::Modified || e.kind == FsEventKind::Created)
                    && e.paths.iter().any(|p| p.ends_with("existing.txt"))
            });
            assert!(
                has_file_event,
                "Expected Modify or Create event for existing.txt, got: {:?}",
                events
            );
        }

        #[test]
        #[serial]
        #[ignore = "flaky in CI — fs events not reliably delivered"]
        fn test_debouncer_delete_file() {
            let temp_dir = TempDir::new().unwrap();
            let watch_path = dunce::canonicalize(temp_dir.path()).unwrap();

            // Create file before starting watcher
            let test_file = watch_path.join("to_delete.txt");
            fs::write(&test_file, "delete me").unwrap();
            std::thread::sleep(Duration::from_millis(50));

            let config = FsNotifyConfig {
                debounce_ms: TEST_DEBOUNCE_MS,
                ignore_patterns: vec![],
            };

            let (mut rx, _handle) = start_with_retry(watch_path, config).unwrap();

            // Delete the file
            fs::remove_file(&test_file).unwrap();

            let events = collect_events(&mut rx);

            // Should have a Remove event (might also have Modify on some platforms)
            let remove_events: Vec<_> = events
                .iter()
                .filter(|e| e.kind == FsEventKind::Removed)
                .collect();
            // Note: On macOS, delete sometimes shows as Modify first
            let has_remove_or_modify = !remove_events.is_empty()
                || events.iter().any(|e| {
                    e.kind == FsEventKind::Modified
                        && e.paths.iter().any(|p| p.ends_with("to_delete.txt"))
                });
            assert!(
                has_remove_or_modify,
                "Expected Remove or Modify event for deleted file, got: {:?}",
                events
            );
        }

        #[test]
        #[serial]
        fn test_debouncer_rename_file() {
            let temp_dir = TempDir::new().unwrap();
            let watch_path = dunce::canonicalize(temp_dir.path()).unwrap();

            // Create file before starting watcher
            let old_path = watch_path.join("old_name.txt");
            fs::write(&old_path, "rename me").unwrap();
            std::thread::sleep(Duration::from_millis(50));

            let config = FsNotifyConfig {
                debounce_ms: TEST_DEBOUNCE_MS,
                ignore_patterns: vec![],
            };

            let (mut rx, _handle) = start_with_retry(watch_path.clone(), config).unwrap();

            // Rename the file
            let new_path = watch_path.join("new_name.txt");
            fs::rename(&old_path, &new_path).unwrap();

            let events = collect_events(&mut rx);

            // On macOS FSEvents, rename may come as:
            // - Rename event (ideal)
            // - Create event for new file (FSEvents consolidation)
            // - Remove + Create pair
            let has_rename = events.iter().any(|e| e.kind == FsEventKind::Renamed);
            let has_new_file = events.iter().any(|e| {
                (e.kind == FsEventKind::Created || e.kind == FsEventKind::Renamed)
                    && e.paths.iter().any(|p| p.ends_with("new_name.txt"))
            });

            assert!(
                has_rename || has_new_file,
                "Expected Rename event or Create for new file, got: {:?}",
                events
            );
        }

        #[test]
        #[serial]
        #[ignore = "flaky in CI — fs events not reliably delivered"]
        fn test_debouncer_multiple_rapid_creates() {
            let temp_dir = TempDir::new().unwrap();
            let watch_path = dunce::canonicalize(temp_dir.path()).unwrap();

            let config = FsNotifyConfig {
                debounce_ms: 50, // Slightly longer debounce to batch events
                ignore_patterns: vec![],
            };

            let (mut rx, _handle) = start_with_retry(watch_path.clone(), config).unwrap();

            // Create multiple files rapidly
            for i in 0..5 {
                let file = watch_path.join(format!("file_{}.txt", i));
                fs::write(&file, format!("content {}", i)).unwrap();
            }

            // Use longer timeout for batched events
            let events = collect_events_smart(
                &mut rx,
                Duration::from_millis(200),
                Duration::from_millis(50),
            );

            // Should have Create events for all files
            let created_files: std::collections::HashSet<_> = events
                .iter()
                .filter(|e| e.kind == FsEventKind::Created)
                .flat_map(|e| e.paths.iter())
                .filter_map(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string())
                .collect();

            // All 5 files should be in create events
            for i in 0..5 {
                let filename = format!("file_{}.txt", i);
                assert!(
                    created_files.contains(&filename),
                    "Missing create event for {}, got: {:?}",
                    filename,
                    created_files
                );
            }
        }

        #[test]
        #[serial]
        fn test_debouncer_gitignore_respected() {
            let temp_dir = TempDir::new().unwrap();
            let watch_path = dunce::canonicalize(temp_dir.path()).unwrap();

            // Create .gitignore first
            let gitignore = watch_path.join(".gitignore");
            fs::write(&gitignore, "*.log\ntarget/\n").unwrap();

            let config = FsNotifyConfig {
                debounce_ms: TEST_DEBOUNCE_MS,
                ignore_patterns: vec![],
            };

            let (mut rx, _handle) = start_with_retry(watch_path.clone(), config).unwrap();

            // Create an ignored file and a normal file
            let log_file = watch_path.join("debug.log");
            let txt_file = watch_path.join("readme.txt");
            fs::write(&log_file, "log content").unwrap();
            fs::write(&txt_file, "readme content").unwrap();

            let events = collect_events(&mut rx);

            // Should NOT have event for .log file (gitignored)
            let has_log = events
                .iter()
                .any(|e| e.paths.iter().any(|p| p.ends_with(".log")));

            // Should have event for .txt file
            let has_txt = events
                .iter()
                .any(|e| e.paths.iter().any(|p| p.ends_with("readme.txt")));

            assert!(
                !has_log,
                "Should not receive events for gitignored .log files"
            );
            assert!(
                has_txt,
                "Should receive events for non-ignored .txt files, got: {:?}",
                events
            );
        }

        #[test]
        #[serial]
        fn test_debouncer_custom_ignore_patterns() {
            let temp_dir = TempDir::new().unwrap();
            let watch_path = dunce::canonicalize(temp_dir.path()).unwrap();

            let config = FsNotifyConfig {
                debounce_ms: TEST_DEBOUNCE_MS,
                ignore_patterns: vec!["*.tmp".to_string(), "cache/**".to_string()],
            };

            let (mut rx, _handle) = start_with_retry(watch_path.clone(), config).unwrap();

            // Create ignored and non-ignored files
            fs::write(watch_path.join("test.tmp"), "temp").unwrap();
            fs::write(watch_path.join("test.txt"), "text").unwrap();

            let events = collect_events(&mut rx);

            let has_tmp = events
                .iter()
                .any(|e| e.paths.iter().any(|p| p.ends_with("test.tmp")));
            let has_txt = events
                .iter()
                .any(|e| e.paths.iter().any(|p| p.ends_with("test.txt")));

            assert!(!has_tmp, "Should not receive events for *.tmp files");
            assert!(
                has_txt,
                "Should receive events for .txt files, got: {:?}",
                events
            );
        }

        #[test]
        #[serial]
        fn test_debouncer_subdirectory() {
            let temp_dir = TempDir::new().unwrap();
            let watch_path = dunce::canonicalize(temp_dir.path()).unwrap();

            // Create subdirectory
            let sub_dir = watch_path.join("src");
            fs::create_dir(&sub_dir).unwrap();

            let config = FsNotifyConfig {
                debounce_ms: TEST_DEBOUNCE_MS,
                ignore_patterns: vec![],
            };

            let (mut rx, _handle) = start_with_retry(watch_path, config).unwrap();

            // Create file in subdirectory
            let nested_file = sub_dir.join("main.rs");
            fs::write(&nested_file, "fn main() {}").unwrap();

            let events = collect_events(&mut rx);

            // Should have Create event for nested file
            let has_nested = events.iter().any(|e| {
                e.kind == FsEventKind::Created && e.paths.iter().any(|p| p.ends_with("main.rs"))
            });

            assert!(
                has_nested,
                "Should receive Create event for file in subdirectory, got: {:?}",
                events
            );
        }

        #[test]
        #[serial]
        fn test_handle_drop_stops_watcher() {
            let temp_dir = TempDir::new().unwrap();
            let watch_path = dunce::canonicalize(temp_dir.path()).unwrap();

            let config = FsNotifyConfig {
                debounce_ms: TEST_DEBOUNCE_MS,
                ..Default::default()
            };

            let (mut rx, handle) = start_with_retry(watch_path.clone(), config).unwrap();
            let _ = collect_events(&mut rx); // drain startup stragglers

            // Drop joins the watcher thread, which drops the debouncer and the
            // event sender. Run it on a watchdog thread so a broken Shutdown
            // path (hung join) fails fast as an assertion rather than hanging
            // the whole test/CI.
            let dropper = std::thread::spawn(move || drop(handle));
            let drop_deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !dropper.is_finished() {
                assert!(
                    std::time::Instant::now() < drop_deadline,
                    "FsNotifyHandle::drop did not return within 5s — shutdown is broken"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            dropper.join().unwrap();

            // The receiver must observe disconnection within a bounded time —
            // this is what proves the watcher actually stopped.
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            let disconnected = loop {
                match rx.try_recv() {
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break true,
                    Ok(_) => {} // drain any straggler before disconnect
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                        if std::time::Instant::now() >= deadline {
                            break false;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            };
            assert!(
                disconnected,
                "event channel must disconnect after the handle is dropped"
            );

            // And a post-drop write must not surface (watcher is gone) — this
            // would still arrive if shutdown had not torn down the watch.
            fs::write(watch_path.join("after_drop.txt"), "test").unwrap();
            std::thread::sleep(Duration::from_millis(EVENT_WAIT_MS));
            let after = collect_events_smart(
                &mut rx,
                Duration::from_millis(100),
                Duration::from_millis(20),
            );
            assert!(
                !after
                    .iter()
                    .any(|e| e.paths.iter().any(|p| p.ends_with("after_drop.txt"))),
                "no event should surface after the watcher is dropped, got: {after:?}"
            );
        }

        #[test]
        #[serial]
        #[ignore = "flaky in CI — fs events not reliably delivered"]
        fn test_debouncer_negation_pattern_include() {
            // Test that negation patterns (!) override ignore patterns
            let temp_dir = TempDir::new().unwrap();
            let watch_path = dunce::canonicalize(temp_dir.path()).unwrap();

            let config = FsNotifyConfig {
                debounce_ms: TEST_DEBOUNCE_MS,
                // Ignore all .log files except important.log
                ignore_patterns: vec!["*.log".to_string(), "!important.log".to_string()],
            };

            let (mut rx, _handle) = start_with_retry(watch_path.clone(), config).unwrap();

            // Create both ignored and included files
            fs::write(watch_path.join("debug.log"), "debug").unwrap();
            fs::write(watch_path.join("important.log"), "important").unwrap();
            fs::write(watch_path.join("test.txt"), "text").unwrap();

            let events = collect_events(&mut rx);

            let has_debug_log = events
                .iter()
                .any(|e| e.paths.iter().any(|p| p.ends_with("debug.log")));
            let has_important_log = events
                .iter()
                .any(|e| e.paths.iter().any(|p| p.ends_with("important.log")));
            let has_txt = events
                .iter()
                .any(|e| e.paths.iter().any(|p| p.ends_with("test.txt")));

            assert!(!has_debug_log, "debug.log should be ignored");
            assert!(
                has_important_log,
                "important.log should be included via negation"
            );
            assert!(has_txt, "test.txt should be included");
        }

        #[test]
        #[serial]
        fn test_debouncer_nested_gitignore() {
            // Test that nested .gitignore files are respected
            let temp_dir = TempDir::new().unwrap();
            let watch_path = dunce::canonicalize(temp_dir.path()).unwrap();

            // Create nested directory structure
            let sub_dir = watch_path.join("src");
            fs::create_dir(&sub_dir).unwrap();

            // Root .gitignore ignores *.tmp
            fs::write(watch_path.join(".gitignore"), "*.tmp\n").unwrap();
            // Nested .gitignore ignores *.bak
            fs::write(sub_dir.join(".gitignore"), "*.bak\n").unwrap();

            let config = FsNotifyConfig {
                debounce_ms: TEST_DEBOUNCE_MS,
                ignore_patterns: vec![],
            };

            let (mut rx, _handle) = start_with_retry(watch_path, config).unwrap();

            // Create files
            fs::write(sub_dir.join("test.tmp"), "tmp").unwrap();
            fs::write(sub_dir.join("test.bak"), "bak").unwrap();
            fs::write(sub_dir.join("test.rs"), "rs").unwrap();

            let events = collect_events(&mut rx);

            let has_tmp = events
                .iter()
                .any(|e| e.paths.iter().any(|p| p.ends_with("test.tmp")));
            let has_bak = events
                .iter()
                .any(|e| e.paths.iter().any(|p| p.ends_with("test.bak")));
            let has_rs = events
                .iter()
                .any(|e| e.paths.iter().any(|p| p.ends_with("test.rs")));

            assert!(!has_tmp, "*.tmp should be ignored by root .gitignore");
            assert!(!has_bak, "*.bak should be ignored by nested .gitignore");
            assert!(has_rs, "*.rs should not be ignored, got: {:?}", events);
        }

        #[test]
        #[serial]
        #[ignore] // Flaky on macOS due to recursive watcher behavior
        fn test_debouncer_git_directory_ignored() {
            // .git directory contents should always be ignored
            let temp_dir = TempDir::new().unwrap();
            let watch_path = dunce::canonicalize(temp_dir.path()).unwrap();

            // Create .git directory
            let git_dir = watch_path.join(".git");
            fs::create_dir(&git_dir).unwrap();

            let config = FsNotifyConfig {
                debounce_ms: TEST_DEBOUNCE_MS,
                ignore_patterns: vec![],
            };

            let (mut rx, _handle) = start_with_retry(watch_path.clone(), config).unwrap();

            // Create files inside .git and outside
            fs::write(git_dir.join("config"), "git config").unwrap();
            fs::write(watch_path.join("README.md"), "readme").unwrap();

            let events = collect_events(&mut rx);

            let has_git_config = events
                .iter()
                .any(|e| e.paths.iter().any(|p| p.to_string_lossy().contains(".git")));
            let has_readme = events
                .iter()
                .any(|e| e.paths.iter().any(|p| p.ends_with("README.md")));

            assert!(!has_git_config, ".git directory contents should be ignored");
            assert!(
                has_readme,
                "README.md should be included, got: {:?}",
                events
            );
        }

        #[test]
        #[serial]
        #[ignore = "flaky in CI — fs events not reliably delivered"]
        fn test_debouncer_create_directory() {
            let temp_dir = TempDir::new().unwrap();
            let watch_path = dunce::canonicalize(temp_dir.path()).unwrap();

            let config = FsNotifyConfig {
                debounce_ms: TEST_DEBOUNCE_MS,
                ignore_patterns: vec![],
            };

            let (mut rx, _handle) = start_with_retry(watch_path.clone(), config).unwrap();

            // Create a directory
            let new_dir = watch_path.join("new_folder");
            fs::create_dir(&new_dir).unwrap();

            let events = collect_events(&mut rx);

            // Should have Create event for the directory
            let create_events: Vec<_> = events
                .iter()
                .filter(|e| e.kind == FsEventKind::Created)
                .collect();

            let has_new_folder = create_events
                .iter()
                .any(|e| e.paths.iter().any(|p| p.ends_with("new_folder")));
            assert!(
                has_new_folder,
                "Should receive Create event for new directory, got: {:?}",
                events
            );
        }

        #[test]
        #[serial]
        fn test_debouncer_deeply_nested_file() {
            let temp_dir = TempDir::new().unwrap();
            let watch_path = dunce::canonicalize(temp_dir.path()).unwrap();

            // Create deeply nested directory structure
            let deep_dir = watch_path.join("a").join("b").join("c").join("d");
            fs::create_dir_all(&deep_dir).unwrap();

            let config = FsNotifyConfig {
                debounce_ms: TEST_DEBOUNCE_MS,
                ignore_patterns: vec![],
            };

            let (mut rx, _handle) = start_with_retry(watch_path, config).unwrap();

            // Create file in deeply nested directory
            let deep_file = deep_dir.join("deep.txt");
            fs::write(&deep_file, "deep content").unwrap();

            let events = collect_events(&mut rx);

            let has_deep_file = events.iter().any(|e| {
                e.kind == FsEventKind::Created && e.paths.iter().any(|p| p.ends_with("deep.txt"))
            });

            assert!(
                has_deep_file,
                "Should receive Create event for deeply nested file, got: {:?}",
                events
            );
        }

        #[test]
        #[serial]
        #[ignore = "flaky in CI — fs events not reliably delivered"]
        fn test_top_level_gitignored_target_never_surfaces() {
            let temp_dir = TempDir::new().unwrap();
            let watch_path = dunce::canonicalize(temp_dir.path()).unwrap();

            // Exclude `target/` via `.git/info/exclude` (not `.gitignore`): the
            // per-event GitignoreCache ignores it, so only watch-level exclusion
            // keeps target paths out — this discriminates the new behavior.
            fs::create_dir_all(watch_path.join(".git/info")).unwrap();
            fs::write(watch_path.join(".git/info/exclude"), "target/\n").unwrap();
            let target = watch_path.join("target");
            fs::create_dir_all(&target).unwrap();
            let src = watch_path.join("src");
            fs::create_dir_all(&src).unwrap();
            std::thread::sleep(Duration::from_millis(50));

            let config = FsNotifyConfig {
                debounce_ms: TEST_DEBOUNCE_MS,
                ignore_patterns: vec![],
            };
            let (mut rx, _handle) = start_with_retry(watch_path.clone(), config).unwrap();

            for i in 0..50 {
                fs::write(target.join(format!("artifact_{i}.o")), "x").unwrap();
            }
            // A non-ignored write proves the watcher is alive and discriminates
            // this assertion from a watcher that simply emits nothing.
            fs::write(src.join("main.rs"), "fn main() {}").unwrap();

            let events = collect_events(&mut rx);

            let has_target = events
                .iter()
                .any(|e| e.paths.iter().any(|p| p.starts_with(&target)));
            let has_src = events
                .iter()
                .any(|e| e.paths.iter().any(|p| p.ends_with("main.rs")));

            assert!(
                !has_target,
                "no event should surface for excluded target/, got: {events:?}"
            );
            assert!(
                has_src,
                "control: src/main.rs event should surface, got: {events:?}"
            );
        }

        #[test]
        #[serial]
        #[ignore = "flaky in CI — fs events not reliably delivered"]
        fn test_fallback_mode_watches_top_level_dir() {
            let temp_dir = TempDir::new().unwrap();
            let watch_path = dunce::canonicalize(temp_dir.path()).unwrap();

            // > cap non-ignored top-level dirs forces the recursive-root
            // fallback, which DOES watch the `.git/info/exclude`d `target/` — the
            // opposite of the fan-out test, pinning the trade-off.
            fs::create_dir_all(watch_path.join(".git/info")).unwrap();
            fs::write(watch_path.join(".git/info/exclude"), "target/\n").unwrap();
            for i in 0..=MAX_TOP_LEVEL_FANOUT {
                fs::create_dir_all(watch_path.join(format!("pkg{i}"))).unwrap();
            }
            let target = watch_path.join("target");
            fs::create_dir_all(&target).unwrap();
            std::thread::sleep(Duration::from_millis(50));

            let config = FsNotifyConfig {
                debounce_ms: TEST_DEBOUNCE_MS,
                ignore_patterns: vec![],
            };
            let (mut rx, _handle) = start_with_retry(watch_path.clone(), config).unwrap();

            // A nested file under a top-level dir must surface — the recursive
            // root watch covers the whole tree in fallback mode.
            fs::write(watch_path.join("pkg0").join("lib.rs"), "// x").unwrap();
            // The excluded top-level dir IS watched in fallback (only fan-out
            // excludes it at the watch level), so its event surfaces here.
            fs::write(target.join("artifact.o"), "x").unwrap();

            let events = collect_events(&mut rx);
            let has_nested = events.iter().any(|e| {
                e.kind == FsEventKind::Created && e.paths.iter().any(|p| p.ends_with("lib.rs"))
            });
            let has_target = events
                .iter()
                .any(|e| e.paths.iter().any(|p| p.starts_with(&target)));
            assert!(
                has_nested,
                "fallback recursive watch must surface nested files, got: {events:?}"
            );
            assert!(
                has_target,
                "fallback watches top-level dirs the fan-out path would exclude, got: {events:?}"
            );
        }

        #[test]
        #[serial]
        #[ignore = "flaky in CI — fs events not reliably delivered"]
        fn test_new_top_level_dir_contents_watched_dynamically() {
            let temp_dir = TempDir::new().unwrap();
            let watch_path = dunce::canonicalize(temp_dir.path()).unwrap();

            let config = FsNotifyConfig {
                debounce_ms: TEST_DEBOUNCE_MS,
                ignore_patterns: vec![],
            };
            let (mut rx, _handle) = start_with_retry(watch_path.clone(), config).unwrap();

            // Created after startup: the root is watched non-recursively, so the
            // watcher must add a recursive watch for this dir dynamically or its
            // contents would be missed.
            let new_dir = watch_path.join("late_crate");
            fs::create_dir(&new_dir).unwrap();

            // Let the dir-create be observed and the recursive watch added.
            std::thread::sleep(Duration::from_millis(EVENT_WAIT_MS));
            let _ = collect_events(&mut rx);

            let nested = new_dir.join("lib.rs");
            fs::write(&nested, "// new").unwrap();

            let events = collect_events(&mut rx);
            let has_nested = events.iter().any(|e| {
                e.kind == FsEventKind::Created && e.paths.iter().any(|p| p.ends_with("lib.rs"))
            });
            assert!(
                has_nested,
                "contents of a dynamically-created top-level dir must be watched, got: {events:?}"
            );
        }

        #[test]
        #[serial]
        #[ignore = "flaky in CI — fs events not reliably delivered"]
        fn test_moved_in_top_level_dir_is_watched() {
            let temp_dir = TempDir::new().unwrap();
            let watch_path = dunce::canonicalize(temp_dir.path()).unwrap();

            // A populated dir prepared OUTSIDE the watch root.
            let outside = TempDir::new().unwrap();
            let staged = outside.path().join("moved_crate");
            fs::create_dir_all(&staged).unwrap();
            fs::write(staged.join("existing.rs"), "// pre-existing").unwrap();

            let config = FsNotifyConfig {
                debounce_ms: TEST_DEBOUNCE_MS,
                ignore_patterns: vec![],
            };
            let (mut rx, _handle) = start_with_retry(watch_path.clone(), config).unwrap();

            // Move it in: surfaces as a Renamed (IN_MOVED_TO) event, which must
            // trigger reconcile and add a recursive watch.
            let dest = watch_path.join("moved_crate");
            fs::rename(&staged, &dest).unwrap();

            std::thread::sleep(Duration::from_millis(EVENT_WAIT_MS));
            let _ = collect_events(&mut rx);

            // A file created after the move must surface (proves it's watched).
            fs::write(dest.join("new.rs"), "// after move").unwrap();
            let events = collect_events(&mut rx);
            let has_new = events.iter().any(|e| {
                e.kind == FsEventKind::Created && e.paths.iter().any(|p| p.ends_with("new.rs"))
            });
            assert!(
                has_new,
                "a moved-in top-level dir must be watched, got: {events:?}"
            );
        }

        #[test]
        #[serial]
        #[ignore = "flaky in CI — fs events not reliably delivered"]
        fn test_deleted_and_recreated_top_level_dir_rewatched() {
            let temp_dir = TempDir::new().unwrap();
            let watch_path = dunce::canonicalize(temp_dir.path()).unwrap();
            let dir = watch_path.join("pkg");
            fs::create_dir(&dir).unwrap();

            let config = FsNotifyConfig {
                debounce_ms: TEST_DEBOUNCE_MS,
                ignore_patterns: vec![],
            };
            let (mut rx, _handle) = start_with_retry(watch_path.clone(), config).unwrap();

            // Delete the watched dir, then recreate it with the same name.
            fs::remove_dir_all(&dir).unwrap();
            std::thread::sleep(Duration::from_millis(EVENT_WAIT_MS));
            let _ = collect_events(&mut rx);
            fs::create_dir(&dir).unwrap();
            std::thread::sleep(Duration::from_millis(EVENT_WAIT_MS));
            let _ = collect_events(&mut rx);

            // The watch must be re-armed: a file in the recreated dir surfaces.
            fs::write(dir.join("again.rs"), "// recreated").unwrap();
            let events = collect_events(&mut rx);
            let has_file = events.iter().any(|e| {
                e.kind == FsEventKind::Created && e.paths.iter().any(|p| p.ends_with("again.rs"))
            });
            assert!(
                has_file,
                "a deleted+recreated top-level dir must be re-watched, got: {events:?}"
            );
        }

        #[test]
        #[serial]
        #[ignore = "flaky in CI — fs events not reliably delivered"]
        #[cfg(unix)]
        fn test_non_canonical_root_dynamic_watch() {
            // The watcher must canonicalize its root so dynamic watching works
            // even when started on a non-canonical (symlinked) path.
            let temp_dir = TempDir::new().unwrap();
            let real = dunce::canonicalize(temp_dir.path()).unwrap();
            let link = real.join("link_root");
            let real_root = real.join("real_root");
            fs::create_dir(&real_root).unwrap();
            std::os::unix::fs::symlink(&real_root, &link).unwrap();

            let config = FsNotifyConfig {
                debounce_ms: TEST_DEBOUNCE_MS,
                ignore_patterns: vec![],
            };
            // Start on the SYMLINKED (non-canonical) path.
            let (mut rx, _handle) = start_with_retry(link.clone(), config).unwrap();

            let new_dir = link.join("late");
            fs::create_dir(&new_dir).unwrap();
            std::thread::sleep(Duration::from_millis(EVENT_WAIT_MS));
            let _ = collect_events(&mut rx);

            fs::write(new_dir.join("f.rs"), "// x").unwrap();
            let events = collect_events(&mut rx);
            // Discriminating: without root canonicalization the event path would
            // be reported under the symlink (`link`), not the real dir.
            let has_canonical_file = events.iter().any(|e| {
                e.paths
                    .iter()
                    .any(|p| p.starts_with(&real_root) && p.ends_with("f.rs"))
            });
            assert!(
                has_canonical_file,
                "dynamic watching must work and report canonical paths on a non-canonical root, got: {events:?}"
            );
        }

        // ── per-dir strategy (Linux default; forced here so it runs on any
        //    platform without process-global env races) ──────────────────────

        /// Watch-count accounting: nested gitignored dirs cost zero watches
        /// and `.git` costs a handful, not one per internal dir.
        #[test]
        #[serial]
        fn test_per_dir_watch_count_excludes_ignored_and_git_internals() {
            let temp = TempDir::new().unwrap();
            let root = dunce::canonicalize(temp.path()).unwrap();
            fs::create_dir_all(root.join(".git/objects/ab")).unwrap();
            fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
            fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
            fs::create_dir_all(root.join("web/src")).unwrap();
            for i in 0..20 {
                fs::create_dir_all(root.join(format!("web/node_modules/pkg{i}/lib"))).unwrap();
            }

            let (_rx, handle) = start_with_retry_strategy(
                root.clone(),
                FsNotifyConfig {
                    debounce_ms: TEST_DEBOUNCE_MS,
                    ignore_patterns: vec![],
                },
                WatchStrategy::PerDir,
            )
            .unwrap();

            // root(1) + web + web/src (2) + .git,.git/refs,.git/refs/heads (3).
            // The 40 node_modules dirs and .git/objects contribute nothing.
            // Depth≥2 dirs arm asynchronously after `ready`, so poll briefly.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while handle.watch_count() != 6 && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(
                handle.watch_count(),
                6,
                "per-dir watch layout must skip ignored trees and git internals"
            );
        }

        /// Files inside a nested gitignored dir generate no events (there is
        /// no watch to generate them), while sibling source dirs still do.
        #[test]
        #[serial]
        #[ignore = "flaky in CI — fs events not reliably delivered"]
        fn test_per_dir_nested_ignored_dir_produces_no_events() {
            let temp = TempDir::new().unwrap();
            let root = dunce::canonicalize(temp.path()).unwrap();
            fs::create_dir_all(root.join(".git")).unwrap();
            fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
            fs::create_dir_all(root.join("web/node_modules/react")).unwrap();
            fs::create_dir_all(root.join("web/src")).unwrap();

            let (mut rx, _handle) = start_with_retry_strategy(
                root.clone(),
                FsNotifyConfig {
                    debounce_ms: TEST_DEBOUNCE_MS,
                    ignore_patterns: vec![],
                },
                WatchStrategy::PerDir,
            )
            .unwrap();

            fs::write(root.join("web/node_modules/react/index.js"), "x").unwrap();
            fs::write(root.join("web/src/app.ts"), "y").unwrap();

            let events = collect_events(&mut rx);
            assert!(
                events
                    .iter()
                    .any(|e| e.paths.iter().any(|p| p.ends_with("app.ts"))),
                "non-ignored file must surface: {events:?}"
            );
            assert!(
                !events.iter().any(|e| e
                    .paths
                    .iter()
                    .any(|p| p.to_string_lossy().contains("node_modules"))),
                "ignored tree must stay silent: {events:?}"
            );
        }

        /// New nested dirs get watched incrementally: files written *after*
        /// the watch attaches still surface, and pre-watch files backfill as
        /// synthetic `Created`s.
        #[test]
        #[serial]
        #[ignore = "flaky in CI — fs events not reliably delivered"]
        fn test_per_dir_new_nested_dir_watched_with_backfill() {
            let temp = TempDir::new().unwrap();
            let root = dunce::canonicalize(temp.path()).unwrap();
            fs::create_dir_all(root.join("src")).unwrap();

            let (mut rx, handle) = start_with_retry_strategy(
                root.clone(),
                FsNotifyConfig {
                    debounce_ms: TEST_DEBOUNCE_MS,
                    ignore_patterns: vec![],
                },
                WatchStrategy::PerDir,
            )
            .unwrap();
            let watches_before = handle.watch_count();

            // Dir + immediate file: the file races the (post-debounce) watch
            // attach, so it must arrive via backfill.
            let deep = root.join("src/gen/v1");
            fs::create_dir_all(&deep).unwrap();
            fs::write(deep.join("early.rs"), "// early").unwrap();

            // Wait out debounce + Update command processing.
            std::thread::sleep(Duration::from_millis(EVENT_WAIT_MS));
            let early_events = collect_events(&mut rx);
            assert!(
                early_events.iter().any(|e| {
                    e.kind == FsEventKind::Created
                        && e.paths.iter().any(|p| p.ends_with("early.rs"))
                }),
                "pre-watch file must backfill as Created: {early_events:?}"
            );

            // A later write proves the incremental watch is armed.
            fs::write(deep.join("late.rs"), "// late").unwrap();
            let late_events = collect_events(&mut rx);
            assert!(
                late_events
                    .iter()
                    .any(|e| e.paths.iter().any(|p| p.ends_with("late.rs"))),
                "new nested dir must be live-watched: {late_events:?}"
            );
            assert!(
                handle.watch_count() > watches_before,
                "watch count must grow for the new subtree"
            );
        }

        /// Deleting a watched subtree shrinks the watch set (bookkeeping and
        /// OS watches both released).
        #[test]
        #[serial]
        #[ignore = "flaky in CI — fs events not reliably delivered"]
        fn test_per_dir_removed_subtree_pruned() {
            let temp = TempDir::new().unwrap();
            let root = dunce::canonicalize(temp.path()).unwrap();
            fs::create_dir_all(root.join("pkg/a/b")).unwrap();

            let (mut rx, handle) = start_with_retry_strategy(
                root.clone(),
                FsNotifyConfig {
                    debounce_ms: TEST_DEBOUNCE_MS,
                    ignore_patterns: vec![],
                },
                WatchStrategy::PerDir,
            )
            .unwrap();
            let watches_before = handle.watch_count();

            fs::remove_dir_all(root.join("pkg")).unwrap();
            std::thread::sleep(Duration::from_millis(EVENT_WAIT_MS));
            let _ = collect_events(&mut rx);

            assert!(
                handle.watch_count() < watches_before,
                "watch count must shrink after subtree removal: {} -> {}",
                watches_before,
                handle.watch_count()
            );
        }
    }

    // ========================================================================
    // Unit tests for merge_events and build_globsets
    // ========================================================================

    mod merge_events_tests {
        use super::*;
        use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};

        fn make_debounced_event(kind: EventKind, paths: Vec<PathBuf>) -> DebouncedEvent {
            DebouncedEvent {
                event: notify::Event {
                    kind,
                    paths,
                    attrs: Default::default(),
                },
                time: std::time::Instant::now(),
            }
        }

        #[test]
        fn test_merge_single_create() {
            let events = vec![make_debounced_event(
                EventKind::Create(CreateKind::File),
                vec![PathBuf::from("/test/file.txt")],
            )];

            let merged = merge_events(events);

            assert_eq!(merged.len(), 1);
            assert_eq!(merged[0].kind, FsEventKind::Created);
            assert_eq!(merged[0].paths.len(), 1);
            assert_eq!(merged[0].paths[0], PathBuf::from("/test/file.txt"));
        }

        #[test]
        fn test_merge_multiple_creates_same_path() {
            // Multiple creates for same path should result in single create
            let events = vec![
                make_debounced_event(
                    EventKind::Create(CreateKind::File),
                    vec![PathBuf::from("/test/file.txt")],
                ),
                make_debounced_event(
                    EventKind::Create(CreateKind::File),
                    vec![PathBuf::from("/test/file.txt")],
                ),
            ];

            let merged = merge_events(events);

            // Should be merged into one event
            let create_count: usize = merged
                .iter()
                .filter(|e| e.kind == FsEventKind::Created)
                .map(|e| e.paths.len())
                .sum();
            assert_eq!(create_count, 1, "Duplicate creates should be merged");
        }

        #[test]
        fn test_merge_create_then_modify() {
            // Create followed by modify should remain Create
            let events = vec![
                make_debounced_event(
                    EventKind::Create(CreateKind::File),
                    vec![PathBuf::from("/test/file.txt")],
                ),
                make_debounced_event(
                    EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
                    vec![PathBuf::from("/test/file.txt")],
                ),
            ];

            let merged = merge_events(events);

            let path_kinds: std::collections::HashMap<_, _> = merged
                .iter()
                .flat_map(|e| e.paths.iter().map(move |p| (p.clone(), e.kind)))
                .collect();

            assert_eq!(
                path_kinds.get(&PathBuf::from("/test/file.txt")),
                Some(&FsEventKind::Created),
                "Create+Modify should remain Create"
            );
        }

        #[test]
        fn test_merge_modify_then_create() {
            // Modify followed by create should become Create
            let events = vec![
                make_debounced_event(
                    EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
                    vec![PathBuf::from("/test/file.txt")],
                ),
                make_debounced_event(
                    EventKind::Create(CreateKind::File),
                    vec![PathBuf::from("/test/file.txt")],
                ),
            ];

            let merged = merge_events(events);

            let path_kinds: std::collections::HashMap<_, _> = merged
                .iter()
                .flat_map(|e| e.paths.iter().map(move |p| (p.clone(), e.kind)))
                .collect();

            assert_eq!(
                path_kinds.get(&PathBuf::from("/test/file.txt")),
                Some(&FsEventKind::Created),
                "Modify+Create should become Create"
            );
        }

        #[test]
        fn test_merge_any_then_remove() {
            // Any event followed by remove should become Remove
            let events = vec![
                make_debounced_event(
                    EventKind::Create(CreateKind::File),
                    vec![PathBuf::from("/test/file.txt")],
                ),
                make_debounced_event(
                    EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
                    vec![PathBuf::from("/test/file.txt")],
                ),
                make_debounced_event(
                    EventKind::Remove(RemoveKind::File),
                    vec![PathBuf::from("/test/file.txt")],
                ),
            ];

            let merged = merge_events(events);

            let path_kinds: std::collections::HashMap<_, _> = merged
                .iter()
                .flat_map(|e| e.paths.iter().map(move |p| (p.clone(), e.kind)))
                .collect();

            assert_eq!(
                path_kinds.get(&PathBuf::from("/test/file.txt")),
                Some(&FsEventKind::Removed),
                "Any+Remove should become Remove"
            );
        }

        #[test]
        fn test_merge_any_then_rename() {
            // Any event followed by rename should become Rename
            let events = vec![
                make_debounced_event(
                    EventKind::Create(CreateKind::File),
                    vec![PathBuf::from("/test/file.txt")],
                ),
                make_debounced_event(
                    EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                    vec![PathBuf::from("/test/file.txt")],
                ),
            ];

            let merged = merge_events(events);

            let path_kinds: std::collections::HashMap<_, _> = merged
                .iter()
                .flat_map(|e| e.paths.iter().map(move |p| (p.clone(), e.kind)))
                .collect();

            assert_eq!(
                path_kinds.get(&PathBuf::from("/test/file.txt")),
                Some(&FsEventKind::Renamed),
                "Any+Rename should become Rename"
            );
        }

        #[test]
        fn test_merge_other_events_filtered() {
            // Other/Access events should be filtered out
            let events = vec![
                make_debounced_event(EventKind::Other, vec![PathBuf::from("/test/other.txt")]),
                make_debounced_event(
                    EventKind::Access(notify::event::AccessKind::Read),
                    vec![PathBuf::from("/test/access.txt")],
                ),
                make_debounced_event(
                    EventKind::Create(CreateKind::File),
                    vec![PathBuf::from("/test/create.txt")],
                ),
            ];

            let merged = merge_events(events);

            // Should only have the Create event
            assert_eq!(merged.len(), 1);
            assert_eq!(merged[0].kind, FsEventKind::Created);
            assert!(merged[0].paths.iter().any(|p| p.ends_with("create.txt")));
        }

        #[test]
        fn test_merge_multiple_different_paths() {
            let events = vec![
                make_debounced_event(
                    EventKind::Create(CreateKind::File),
                    vec![PathBuf::from("/test/a.txt")],
                ),
                make_debounced_event(
                    EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
                    vec![PathBuf::from("/test/b.txt")],
                ),
                make_debounced_event(
                    EventKind::Remove(RemoveKind::File),
                    vec![PathBuf::from("/test/c.txt")],
                ),
            ];

            let merged = merge_events(events);

            let path_kinds: std::collections::HashMap<_, _> = merged
                .iter()
                .flat_map(|e| e.paths.iter().map(move |p| (p.clone(), e.kind)))
                .collect();

            assert_eq!(
                path_kinds.get(&PathBuf::from("/test/a.txt")),
                Some(&FsEventKind::Created)
            );
            assert_eq!(
                path_kinds.get(&PathBuf::from("/test/b.txt")),
                Some(&FsEventKind::Modified)
            );
            assert_eq!(
                path_kinds.get(&PathBuf::from("/test/c.txt")),
                Some(&FsEventKind::Removed)
            );
        }

        #[test]
        fn test_merge_empty_events() {
            let events: Vec<DebouncedEvent> = vec![];
            let merged = merge_events(events);
            assert!(merged.is_empty());
        }

        #[test]
        fn test_merge_groups_by_kind() {
            // Multiple files with same kind should be grouped
            let events = vec![
                make_debounced_event(
                    EventKind::Create(CreateKind::File),
                    vec![PathBuf::from("/test/a.txt")],
                ),
                make_debounced_event(
                    EventKind::Create(CreateKind::File),
                    vec![PathBuf::from("/test/b.txt")],
                ),
                make_debounced_event(
                    EventKind::Create(CreateKind::File),
                    vec![PathBuf::from("/test/c.txt")],
                ),
            ];

            let merged = merge_events(events);

            // All creates should be grouped into one RawFsEvent
            let create_events: Vec<_> = merged
                .iter()
                .filter(|e| e.kind == FsEventKind::Created)
                .collect();
            assert_eq!(create_events.len(), 1);
            assert_eq!(create_events[0].paths.len(), 3);
        }
    }

    mod build_globsets_tests {
        use super::*;

        #[test]
        fn test_build_globsets_empty() {
            let (ignore, include) = build_globsets(&[]);
            assert!(ignore.is_none());
            assert!(include.is_none());
        }

        #[test]
        fn test_build_globsets_ignore_pattern() {
            let (ignore, include) = build_globsets(&["*.log".to_string()]);

            assert!(ignore.is_some());
            assert!(include.is_none());

            let ignore_set = ignore.unwrap();
            assert!(ignore_set.is_match("debug.log"));
            assert!(ignore_set.is_match("path/to/error.log"));
            assert!(!ignore_set.is_match("readme.txt"));
        }

        #[test]
        fn test_build_globsets_negation_pattern() {
            let (ignore, include) = build_globsets(&["!important.log".to_string()]);

            assert!(ignore.is_none());
            assert!(include.is_some());

            let include_set = include.unwrap();
            assert!(include_set.is_match("important.log"));
            assert!(include_set.is_match("path/to/important.log"));
            assert!(!include_set.is_match("other.log"));
        }

        #[test]
        fn test_build_globsets_mixed_patterns() {
            let (ignore, include) = build_globsets(&[
                "*.log".to_string(),
                "*.tmp".to_string(),
                "!important.log".to_string(),
                "!keep.tmp".to_string(),
            ]);

            assert!(ignore.is_some());
            assert!(include.is_some());

            let ignore_set = ignore.unwrap();
            let include_set = include.unwrap();

            // Ignore patterns
            assert!(ignore_set.is_match("debug.log"));
            assert!(ignore_set.is_match("cache.tmp"));

            // Include patterns (negations)
            assert!(include_set.is_match("important.log"));
            assert!(include_set.is_match("keep.tmp"));
        }

        #[test]
        fn test_build_globsets_directory_pattern() {
            let (ignore, _) = build_globsets(&["target/**".to_string()]);

            assert!(ignore.is_some());
            let ignore_set = ignore.unwrap();

            assert!(ignore_set.is_match("target/debug/binary"));
            assert!(ignore_set.is_match("target/release/lib.so"));
            assert!(!ignore_set.is_match("src/main.rs"));
        }

        #[test]
        fn test_build_globsets_absolute_pattern() {
            // Patterns starting with / shouldn't get **/ prepended
            let (ignore, _) = build_globsets(&["/root_only.txt".to_string()]);

            assert!(ignore.is_some());
            let ignore_set = ignore.unwrap();

            // Note: The pattern /root_only.txt matches paths ending with /root_only.txt
            // This is slightly different from gitignore semantics but works for our use case
            assert!(ignore_set.is_match("/root_only.txt"));
        }

        #[test]
        fn test_build_globsets_already_prefixed() {
            // Patterns already starting with **/ shouldn't get double-prefixed
            let (ignore, _) = build_globsets(&["**/node_modules/**".to_string()]);

            assert!(ignore.is_some());
            let ignore_set = ignore.unwrap();

            assert!(ignore_set.is_match("node_modules/package/index.js"));
            assert!(ignore_set.is_match("frontend/node_modules/lodash/index.js"));
        }

        #[test]
        fn test_build_globsets_complex_patterns() {
            let (ignore, _) = build_globsets(&[
                "*.{log,tmp,bak}".to_string(),
                "__pycache__/**".to_string(),
                ".DS_Store".to_string(),
            ]);

            assert!(ignore.is_some());
            let ignore_set = ignore.unwrap();

            assert!(ignore_set.is_match("debug.log"));
            assert!(ignore_set.is_match("file.tmp"));
            assert!(ignore_set.is_match("backup.bak"));
            assert!(ignore_set.is_match("__pycache__/module.pyc"));
            assert!(ignore_set.is_match(".DS_Store"));
            assert!(ignore_set.is_match("subdir/.DS_Store"));
        }
    }

    mod config_tests {
        use super::*;

        #[test]
        fn test_config_default() {
            let config = FsNotifyConfig::default();
            assert_eq!(config.debounce_ms, DEBOUNCE_MS);
            assert!(config.ignore_patterns.is_empty());
        }

        #[test]
        fn test_config_struct_literal() {
            let config = FsNotifyConfig {
                debounce_ms: 250,
                ignore_patterns: vec!["target/**".to_string()],
            };
            assert_eq!(config.debounce_ms, 250);
            assert_eq!(config.ignore_patterns, vec!["target/**".to_string()]);
        }

        #[test]
        fn is_git_path_for_watcher_lets_index_lock_through() {
            assert!(is_git_path_for_watcher(Path::new("/r/.git/index.lock")));
            assert!(is_git_path_for_watcher(Path::new("/r/.git/HEAD")));
            assert!(!is_git_path_for_watcher(Path::new(
                "/r/.git/COMMIT_EDITMSG"
            )));
            assert!(!is_git_path_for_watcher(Path::new("/r/src/main.rs")));
        }

        #[test]
        fn is_sl_path_for_watcher_lets_only_wlock_through() {
            assert!(is_sl_path_for_watcher(Path::new("/r/.sl/wlock")));
            // dirstate is read on demand, never watched — must NOT pass.
            assert!(!is_sl_path_for_watcher(Path::new("/r/.sl/dirstate")));
            assert!(!is_sl_path_for_watcher(Path::new("/r/.sl/store/lock")));
            assert!(!is_sl_path_for_watcher(Path::new("/r/src/main.rs")));
        }

        #[test]
        fn gitignore_cache_is_ignored_handles_sl_like_git() {
            let mut cache = GitignoreCache::default();
            // Only `.sl/wlock` reaches the source; everything else under `.sl`
            // (notably `dirstate`, read on demand) stays ignored.
            assert!(!cache.is_ignored(Path::new("/ws/.sl/wlock"), true, true));
            assert!(cache.is_ignored(Path::new("/ws/.sl/dirstate"), true, true));
            assert!(cache.is_ignored(Path::new("/ws/.sl/store/lock"), true, true));
            // With watch_vcs off, even wlock is ignored (mirrors `.git`).
            assert!(cache.is_ignored(Path::new("/ws/.sl/wlock"), false, true));
            // Kill-switch off: the `.sl` arm is skipped, so `.sl/*` is no longer
            // specially ignored here (it is dropped structurally in the source).
            assert!(!cache.is_ignored(Path::new("/ws/.sl/dirstate"), true, false));
        }

        #[test]
        fn test_gitignore_cache_is_ignored_with_watch_vcs() {
            let mut cache = GitignoreCache::default();

            // Without watch_vcs, all git paths are ignored.
            assert!(cache.is_ignored(Path::new("/workspace/.git/index"), false, true));
            assert!(cache.is_ignored(Path::new("/workspace/.git/HEAD"), false, true));
            assert!(cache.is_ignored(Path::new("/workspace/.git/objects/123"), false, true));

            // With watch_vcs, watched git files are NOT ignored.
            assert!(!cache.is_ignored(Path::new("/workspace/.git/index"), true, true));
            assert!(!cache.is_ignored(Path::new("/workspace/.git/HEAD"), true, true));
            assert!(!cache.is_ignored(Path::new("/workspace/.git/refs/heads/main"), true, true));
            assert!(!cache.is_ignored(Path::new("/workspace/.git/packed-refs"), true, true));

            // Other git paths are still ignored even with watch_vcs.
            assert!(cache.is_ignored(Path::new("/workspace/.git/objects/123"), true, true));
            assert!(cache.is_ignored(Path::new("/workspace/.git/COMMIT_EDITMSG"), true, true));
        }
    }

    mod select_top_level_watch_dirs_tests {
        use super::*;
        use std::fs;
        use tempfile::TempDir;

        fn contains_name(dirs: &[PathBuf], name: &str) -> bool {
            dirs.iter()
                .any(|d| d.file_name().is_some_and(|n| n == name))
        }

        #[test]
        fn returns_only_immediate_child_dirs() {
            let temp = TempDir::new().unwrap();
            let root = temp.path();
            fs::create_dir_all(root.join("src/utils")).unwrap();
            fs::create_dir_all(root.join("tests")).unwrap();
            fs::write(root.join("README.md"), "x").unwrap();

            let dirs = select_top_level_watch_dirs(root, &None, &None);

            assert!(contains_name(&dirs, "src"), "src must be watched: {dirs:?}");
            assert!(
                contains_name(&dirs, "tests"),
                "tests must be watched: {dirs:?}"
            );
            // Depth-2 dirs are reached via the child's recursive watch, not here.
            assert!(
                !contains_name(&dirs, "utils"),
                "nested utils must not be a top-level entry: {dirs:?}"
            );
            // The root is watched non-recursively and never returned here.
            assert!(!dirs.iter().any(|d| d == root), "root must not be returned");
            // Files are covered by the root watch, not watched directly.
            assert!(!contains_name(&dirs, "README.md"));
        }

        #[test]
        fn excludes_git_directory() {
            let temp = TempDir::new().unwrap();
            let root = temp.path();
            fs::create_dir_all(root.join(".git/objects")).unwrap();
            fs::create_dir_all(root.join("src")).unwrap();

            let dirs = select_top_level_watch_dirs(root, &None, &None);

            assert!(contains_name(&dirs, "src"));
            assert!(
                !contains_name(&dirs, ".git"),
                ".git is watched separately, not as a recursive child: {dirs:?}"
            );
        }

        #[test]
        fn excludes_sl_directory() {
            // `.sl` is watched separately (non-recursively), never as a
            // recursive workspace child — same treatment as `.git`.
            let temp = TempDir::new().unwrap();
            let root = temp.path();
            fs::create_dir_all(root.join(".sl/store")).unwrap();
            fs::create_dir_all(root.join("src")).unwrap();

            let dirs = select_top_level_watch_dirs(root, &None, &None);

            assert!(contains_name(&dirs, "src"));
            assert!(
                !contains_name(&dirs, ".sl"),
                ".sl must not be a recursive child (avoids .sl/store churn): {dirs:?}"
            );
        }

        #[test]
        fn excludes_gitignored_target_but_keeps_similar_names() {
            let temp = TempDir::new().unwrap();
            let root = temp.path();
            // `.git` is required for WalkBuilder to honor `.gitignore`.
            fs::create_dir_all(root.join(".git")).unwrap();
            fs::write(root.join(".gitignore"), "target/\n").unwrap();
            fs::create_dir_all(root.join("src")).unwrap();
            fs::create_dir_all(root.join("target/debug")).unwrap();
            fs::create_dir_all(root.join("target_data")).unwrap();

            let dirs = select_top_level_watch_dirs(root, &None, &None);

            assert!(contains_name(&dirs, "src"), "src must be watched: {dirs:?}");
            // The whole point of the change: a gitignored top-level dir is
            // never watched, so its churn never reaches the pipeline.
            assert!(
                !contains_name(&dirs, "target"),
                "gitignored target must be excluded: {dirs:?}"
            );
            // A different dir that merely shares a prefix must not be excluded.
            assert!(
                contains_name(&dirs, "target_data"),
                "non-ignored target_data must be watched: {dirs:?}"
            );
        }

        #[test]
        fn honors_custom_ignore_patterns() {
            let temp = TempDir::new().unwrap();
            let root = temp.path();
            fs::create_dir_all(root.join("src")).unwrap();
            fs::create_dir_all(root.join("vendor")).unwrap();

            let (ignore, _) = build_globsets(&["**/vendor".to_string()]);
            let dirs = select_top_level_watch_dirs(root, &ignore, &None);

            assert!(contains_name(&dirs, "src"));
            assert!(
                !contains_name(&dirs, "vendor"),
                "custom-ignored vendor must be excluded: {dirs:?}"
            );
        }

        #[test]
        fn custom_include_overrides_custom_ignore() {
            let temp = TempDir::new().unwrap();
            let root = temp.path();
            fs::create_dir_all(root.join("src")).unwrap();
            fs::create_dir_all(root.join("vendor")).unwrap();

            let (ignore, include) =
                build_globsets(&["**/vendor".to_string(), "!**/vendor".to_string()]);

            // Control: ignore alone excludes vendor.
            let ignored_only = select_top_level_watch_dirs(root, &ignore, &None);
            assert!(
                !contains_name(&ignored_only, "vendor"),
                "control: ignore alone must exclude vendor: {ignored_only:?}"
            );

            // Include overrides the ignore for the same path.
            let dirs = select_top_level_watch_dirs(root, &ignore, &include);
            assert!(
                contains_name(&dirs, "vendor"),
                "include must override ignore: {dirs:?}"
            );
            assert!(contains_name(&dirs, "src"));
        }

        #[test]
        fn empty_root_returns_no_dirs() {
            let temp = TempDir::new().unwrap();
            let dirs = select_top_level_watch_dirs(temp.path(), &None, &None);
            assert!(dirs.is_empty(), "empty root has no child dirs: {dirs:?}");
        }

        #[test]
        fn files_only_returns_no_dirs() {
            let temp = TempDir::new().unwrap();
            let root = temp.path();
            fs::write(root.join("a.txt"), "x").unwrap();
            fs::write(root.join("b.txt"), "x").unwrap();
            let dirs = select_top_level_watch_dirs(root, &None, &None);
            assert!(
                dirs.is_empty(),
                "top-level files are covered by the root watch: {dirs:?}"
            );
        }

        #[test]
        fn hidden_non_ignored_dir_is_included() {
            let temp = TempDir::new().unwrap();
            let root = temp.path();
            fs::create_dir_all(root.join(".config")).unwrap();
            fs::create_dir_all(root.join("src")).unwrap();
            let dirs = select_top_level_watch_dirs(root, &None, &None);
            assert!(
                contains_name(&dirs, ".config"),
                "hidden non-ignored dirs are watched: {dirs:?}"
            );
            assert!(contains_name(&dirs, "src"));
        }

        #[test]
        fn watches_children_even_when_root_is_under_a_gitignored_path() {
            // The user explicitly chose a cwd that an ancestor .gitignore marks
            // ignored; its children must still be watched.
            let temp = TempDir::new().unwrap();
            let repo = temp.path();
            fs::create_dir_all(repo.join(".git")).unwrap();
            fs::write(repo.join(".gitignore"), "build/\n").unwrap();
            let watch_root = repo.join("build");
            fs::create_dir_all(watch_root.join("src")).unwrap();
            fs::create_dir_all(watch_root.join("out")).unwrap();

            let dirs = select_top_level_watch_dirs(&watch_root, &None, &None);

            assert!(
                contains_name(&dirs, "src"),
                "children of a gitignored root must still be watched: {dirs:?}"
            );
            assert!(
                contains_name(&dirs, "out"),
                "children of a gitignored root must still be watched: {dirs:?}"
            );
        }

        #[cfg(unix)]
        #[test]
        fn symlinked_child_dir_is_skipped() {
            let temp = TempDir::new().unwrap();
            let root = temp.path();
            fs::create_dir_all(root.join("real")).unwrap();
            // A symlinked dir would, if recursively watched, leave the
            // workspace; it must be skipped.
            std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();

            let dirs = select_top_level_watch_dirs(root, &None, &None);

            assert!(contains_name(&dirs, "real"), "real dir watched: {dirs:?}");
            assert!(
                !contains_name(&dirs, "link"),
                "symlinked child must be skipped: {dirs:?}"
            );
        }

        #[test]
        fn excludes_dir_ignored_only_by_git_info_exclude() {
            // `.git/info/exclude` is honored at the watch level (WalkBuilder)
            // but NOT by the per-event GitignoreCache — so this exercises the
            // stronger watch-level coverage specifically.
            let temp = TempDir::new().unwrap();
            let root = temp.path();
            fs::create_dir_all(root.join(".git/info")).unwrap();
            fs::write(root.join(".git/info/exclude"), "generated/\n").unwrap();
            fs::create_dir_all(root.join("src")).unwrap();
            fs::create_dir_all(root.join("generated")).unwrap();

            let dirs = select_top_level_watch_dirs(root, &None, &None);

            assert!(contains_name(&dirs, "src"), "src watched: {dirs:?}");
            assert!(
                !contains_name(&dirs, "generated"),
                ".git/info/exclude'd dir must be excluded at the watch level: {dirs:?}"
            );
        }

        #[test]
        fn gitignore_wins_over_custom_include_at_watch_level() {
            // WalkBuilder never yields a gitignored child, so a negation cannot
            // re-add a gitignored top-level dir at the watch level.
            let temp = TempDir::new().unwrap();
            let root = temp.path();
            fs::create_dir_all(root.join(".git")).unwrap();
            fs::write(root.join(".gitignore"), "vendor/\n").unwrap();
            fs::create_dir_all(root.join("vendor")).unwrap();
            fs::create_dir_all(root.join("src")).unwrap();

            let (_, include) = build_globsets(&["!**/vendor".to_string()]);
            let dirs = select_top_level_watch_dirs(root, &None, &include);

            assert!(contains_name(&dirs, "src"));
            assert!(
                !contains_name(&dirs, "vendor"),
                "gitignore wins over custom_include at the watch level: {dirs:?}"
            );
        }
    }

    mod per_dir_tests {
        use super::*;
        use std::fs;
        use tempfile::TempDir;

        fn rel(dirs: &[PathBuf], root: &Path) -> Vec<String> {
            dirs.iter()
                .map(|d| {
                    d.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/")
                })
                .collect()
        }

        /// The core regression: gitignored dirs nested *below* the top level
        /// (invisible to the fan-out selector) are pruned at every depth.
        #[test]
        fn select_per_dir_prunes_nested_gitignored_dirs() {
            let temp = TempDir::new().unwrap();
            let root = temp.path();
            fs::create_dir_all(root.join(".git")).unwrap();
            fs::write(root.join(".gitignore"), "node_modules/\ntarget/\n").unwrap();
            fs::create_dir_all(root.join("web/src/components")).unwrap();
            fs::create_dir_all(root.join("web/node_modules/react/lib")).unwrap();
            fs::create_dir_all(root.join("svc/target/debug/deps")).unwrap();
            fs::create_dir_all(root.join("svc/src")).unwrap();

            let dirs = select_per_dir_watch_dirs(root, &None, &None);
            let names = rel(&dirs, root);

            for expected in ["web", "web/src", "web/src/components", "svc", "svc/src"] {
                assert!(
                    names.contains(&expected.to_string()),
                    "{expected}: {names:?}"
                );
            }
            assert!(
                !names.iter().any(|n| n.contains("node_modules")),
                "nested node_modules must be pruned: {names:?}"
            );
            assert!(
                !names.iter().any(|n| n.contains("target")),
                "nested target must be pruned: {names:?}"
            );
            assert!(
                !names.iter().any(|n| n.contains(".git")),
                ".git is watched separately: {names:?}"
            );
        }

        /// Shallow-first ordering means a watch budget sheds the deepest dirs.
        #[test]
        fn select_per_dir_orders_shallow_first() {
            let temp = TempDir::new().unwrap();
            let root = temp.path();
            fs::create_dir_all(root.join("a/b/c/d")).unwrap();
            fs::create_dir_all(root.join("z")).unwrap();

            let dirs = select_per_dir_watch_dirs(root, &None, &None);
            let depths: Vec<usize> = dirs.iter().map(|d| d.components().count()).collect();
            let mut sorted = depths.clone();
            sorted.sort_unstable();
            assert_eq!(depths, sorted, "must be shallow-first: {dirs:?}");
        }

        /// Custom glob pruning applies at every depth, like the top-level
        /// selector's semantics.
        #[test]
        fn select_per_dir_applies_custom_globs() {
            let temp = TempDir::new().unwrap();
            let root = temp.path();
            fs::create_dir_all(root.join("keep/skipme/deep")).unwrap();
            fs::create_dir_all(root.join("keep/sub")).unwrap();

            let (ignore, include) = build_globsets(&["skipme".to_string()]);
            let dirs = select_per_dir_watch_dirs(root, &ignore, &include);
            let names = rel(&dirs, root);

            assert!(names.contains(&"keep".to_string()));
            assert!(names.contains(&"keep/sub".to_string()));
            assert!(
                !names.iter().any(|n| n.contains("skipme")),
                "custom-ignored subtree must be pruned: {names:?}"
            );
        }

        /// Symlinked dirs are never watched (or followed), so watches can't
        /// leave the workspace.
        #[cfg(unix)]
        #[test]
        fn select_per_dir_skips_symlinked_dirs() {
            let temp = TempDir::new().unwrap();
            let outside = TempDir::new().unwrap();
            let root = temp.path();
            fs::create_dir_all(root.join("real")).unwrap();
            std::os::unix::fs::symlink(outside.path(), root.join("linked")).unwrap();

            let dirs = select_per_dir_watch_dirs(root, &None, &None);
            let names = rel(&dirs, root);
            assert!(names.contains(&"real".to_string()));
            assert!(
                !names.iter().any(|n| n.contains("linked")),
                "symlinked dir must not be watched: {names:?}"
            );
        }

        /// `.git` gets surgical watches: non-recursive `.git` + `refs`,
        /// recursive `refs/heads` + `refs/tags` — never `objects/`/`modules/`.
        #[test]
        fn per_dir_git_watches_are_surgical() {
            let temp = TempDir::new().unwrap();
            let gd = temp.path().join(".git");
            for d in [
                "objects/ab",
                "modules/sub/objects/cd",
                "refs/heads/feature",
                "refs/tags",
                "refs/remotes/origin",
            ] {
                fs::create_dir_all(gd.join(d)).unwrap();
            }

            let watches = per_dir_git_watches(&gd);
            let paths: Vec<(String, RecursiveMode)> = watches
                .iter()
                .map(|(p, m)| {
                    (
                        p.strip_prefix(temp.path())
                            .unwrap()
                            .to_string_lossy()
                            .replace('\\', "/"),
                        *m,
                    )
                })
                .collect();

            assert!(paths.contains(&(".git".into(), RecursiveMode::NonRecursive)));
            assert!(paths.contains(&(".git/refs".into(), RecursiveMode::NonRecursive)));
            assert!(paths.contains(&(".git/refs/heads".into(), RecursiveMode::Recursive)));
            assert!(paths.contains(&(".git/refs/tags".into(), RecursiveMode::Recursive)));
            assert!(
                !paths.iter().any(|(p, _)| p.contains("objects")
                    || p.contains("modules")
                    || p.contains("remotes")),
                "objects/modules/remotes must never be watched: {paths:?}"
            );
        }

        /// Worktree git dirs (no `refs/`) degrade to just the non-recursive
        /// dir watch covering their `HEAD`/`index`.
        #[test]
        fn per_dir_git_watches_worktree_gitdir() {
            let temp = TempDir::new().unwrap();
            let gd = temp.path().join(".git/worktrees/wt");
            fs::create_dir_all(&gd).unwrap();

            let watches = per_dir_git_watches(&gd);
            assert_eq!(
                watches,
                vec![(gd.clone(), RecursiveMode::NonRecursive)],
                "worktree gitdir has no refs/: {watches:?}"
            );
        }

        #[test]
        fn scan_updates_classifies_created_dirs_and_files() {
            let temp = TempDir::new().unwrap();
            let dir = temp.path().join("newdir");
            let file = temp.path().join("newfile");
            fs::create_dir(&dir).unwrap();
            fs::write(&file, "x").unwrap();

            let mut pruned = Vec::new();
            let mut added = Vec::new();
            scan_per_dir_updates(
                FsEventKind::Created,
                &[dir.clone(), file.clone()],
                &mut pruned,
                &mut added,
            );
            assert_eq!(added, vec![dir.clone()], "only dirs become subtree adds");
            // Structural event on an existing dir also prunes (re-arm for the
            // delete+recreate-within-one-debounce case); the file prune
            // candidate is rejected O(1) by the watcher thread.
            assert_eq!(pruned, vec![dir, file]);
        }

        #[test]
        fn scan_updates_prunes_removed_paths() {
            let mut pruned = Vec::new();
            let mut added = Vec::new();
            scan_per_dir_updates(
                FsEventKind::Removed,
                &[PathBuf::from("/gone/dir")],
                &mut pruned,
                &mut added,
            );
            assert_eq!(pruned, vec![PathBuf::from("/gone/dir")]);
            assert!(added.is_empty());
        }

        /// FSEvents can coalesce a subtree removal into `Modified` on the
        /// vanished parent — state, not kind, must drive the prune.
        #[test]
        fn scan_updates_prunes_vanished_dir_on_modified() {
            let mut pruned = Vec::new();
            let mut added = Vec::new();
            scan_per_dir_updates(
                FsEventKind::Modified,
                &[PathBuf::from("/vanished/pkg")],
                &mut pruned,
                &mut added,
            );
            assert_eq!(pruned, vec![PathBuf::from("/vanished/pkg")]);
            assert!(added.is_empty());
        }

        /// A `Modified` on an existing dir (metadata touch) is an add
        /// candidate only — no re-arm prune, so the watcher thread's
        /// `contains` check makes it a no-op.
        #[test]
        fn scan_updates_modified_existing_dir_is_add_candidate_only() {
            let temp = TempDir::new().unwrap();
            let dir = temp.path().join("d");
            fs::create_dir(&dir).unwrap();

            let mut pruned = Vec::new();
            let mut added = Vec::new();
            scan_per_dir_updates(
                FsEventKind::Modified,
                std::slice::from_ref(&dir),
                &mut pruned,
                &mut added,
            );
            assert_eq!(added, vec![dir]);
            assert!(pruned.is_empty(), "no re-arm for non-structural events");
        }

        /// Rename shapes (`From`/`To`/`Both`) classify by on-disk state: the
        /// vanished old name prunes, the existing new name adds (with re-arm).
        #[test]
        fn scan_updates_classifies_renames_by_disk_state() {
            let temp = TempDir::new().unwrap();
            let new_dir = temp.path().join("new");
            fs::create_dir(&new_dir).unwrap();
            let old_dir = temp.path().join("old"); // never created — "moved away"

            let mut pruned = Vec::new();
            let mut added = Vec::new();
            scan_per_dir_updates(
                FsEventKind::Renamed,
                &[old_dir.clone(), new_dir.clone()],
                &mut pruned,
                &mut added,
            );
            assert_eq!(pruned, vec![old_dir, new_dir.clone()]);
            assert_eq!(added, vec![new_dir]);
        }

        /// Symlinked dir created at runtime must not become a subtree add.
        #[cfg(unix)]
        #[test]
        fn scan_updates_skips_symlinked_dirs() {
            let temp = TempDir::new().unwrap();
            let target = temp.path().join("t");
            fs::create_dir(&target).unwrap();
            let link = temp.path().join("link");
            std::os::unix::fs::symlink(&target, &link).unwrap();

            let mut pruned = Vec::new();
            let mut added = Vec::new();
            scan_per_dir_updates(
                FsEventKind::Created,
                std::slice::from_ref(&link),
                &mut pruned,
                &mut added,
            );
            assert!(added.is_empty(), "symlink must not be added: {added:?}");
        }
    }

    mod helper_tests {
        use super::*;
        use std::fs;
        use tempfile::TempDir;

        #[test]
        fn is_top_level_child_distinguishes_levels() {
            let root = Path::new("/work/repo");
            assert!(is_top_level_child(Path::new("/work/repo/src"), root));
            assert!(is_top_level_child(Path::new("/work/repo/file.rs"), root));
            // Grandchildren, the root itself, and outside paths are not.
            assert!(!is_top_level_child(
                Path::new("/work/repo/src/main.rs"),
                root
            ));
            assert!(!is_top_level_child(root, root));
            assert!(!is_top_level_child(Path::new("/work/other"), root));
        }

        #[test]
        fn event_triggers_reconcile_only_for_top_level_structural() {
            let root = Path::new("/r");
            let child = [PathBuf::from("/r/pkg")];
            let nested = [PathBuf::from("/r/pkg/sub")];

            // Structural change to a direct child → reconcile.
            for kind in [
                FsEventKind::Created,
                FsEventKind::Removed,
                FsEventKind::Renamed,
            ] {
                assert!(event_triggers_reconcile(kind, &child, root), "{kind:?}");
            }
            // A bare modify is not structural.
            assert!(!event_triggers_reconcile(
                FsEventKind::Modified,
                &child,
                root
            ));
            // Structural but deeper than a direct child.
            assert!(!event_triggers_reconcile(
                FsEventKind::Created,
                &nested,
                root
            ));
            // Any top-level child in the batch is enough (drives the
            // one-reconcile-per-batch coalescing in the callback).
            let mixed = [PathBuf::from("/r/pkg/sub"), PathBuf::from("/r/newpkg")];
            assert!(event_triggers_reconcile(FsEventKind::Created, &mixed, root));
        }

        #[test]
        fn find_git_dir_discovers_real_repo_and_from_subdir() {
            // A real repo's `.git` is found from the root and from a subdir.
            let temp = TempDir::new().unwrap();
            git2::Repository::init(temp.path()).unwrap();

            let gd = find_git_dir(temp.path()).expect("repo .git found");
            assert!(gd.ends_with(".git") && gd.is_dir(), "got {gd:?}");

            let sub = temp.path().join("crates/inner");
            fs::create_dir_all(&sub).unwrap();
            let gd_sub = find_git_dir(&sub).expect(".git found from subdir");
            assert!(
                gd_sub.ends_with(".git") && gd_sub.is_dir(),
                "got {gd_sub:?}"
            );
        }

        #[test]
        fn find_git_dir_none_when_no_repo() {
            // Hermetic: we create no `.git`, so `find_git_dir` must not return one
            // inside our tree (an ancestor repo's `.git`, outside it, is fine).
            let temp = TempDir::new().unwrap();
            let root = dunce::canonicalize(temp.path()).unwrap();
            let deep = root.join("no/git/here");
            fs::create_dir_all(&deep).unwrap();
            let result = find_git_dir(&deep);
            assert!(
                result.as_ref().is_none_or(|p| !p.starts_with(&root)),
                "must not find a .git inside a tree that contains none, got {result:?}"
            );
        }

        #[test]
        fn find_git_dir_rejects_bogus_gitlink() {
            // A planted `.git` file pointing at a non-git dir must NOT be
            // resolved/watched — git validation rejects it.
            let external = TempDir::new().unwrap();
            let proj = TempDir::new().unwrap();
            fs::write(
                proj.path().join(".git"),
                format!("gitdir: {}\n", external.path().display()),
            )
            .unwrap();

            let resolved = find_git_dir(proj.path());
            let external_canon = dunce::canonicalize(external.path()).unwrap();
            assert!(
                resolved.as_deref() != Some(external_canon.as_path()),
                "bogus gitlink target must not be watched, got {resolved:?}"
            );
        }

        #[cfg(unix)]
        #[test]
        fn find_git_dir_rejects_symlinked_git_to_external_dir() {
            // A `.git` SYMLINK to an external (non-git) dir must NOT be followed
            // and watched: the cheap dir branch is gated on a real (non-symlink)
            // dir, and git validation rejects the target.
            let external = TempDir::new().unwrap(); // stands in for ~/.ssh, /etc
            let proj = TempDir::new().unwrap();
            std::os::unix::fs::symlink(external.path(), proj.path().join(".git")).unwrap();

            let resolved = find_git_dir(proj.path());
            let external_canon = dunce::canonicalize(external.path()).unwrap();
            assert!(
                resolved.as_deref() != Some(external_canon.as_path()),
                "symlinked .git to an external dir must not be watched, got {resolved:?}"
            );
        }

        #[test]
        fn find_git_dir_resolves_legitimate_gitlink() {
            // A `.git` FILE pointing at a real git dir (the worktree / submodule
            // layout) must resolve to that gitdir.
            let temp = TempDir::new().unwrap();
            let main = temp.path().join("main");
            fs::create_dir_all(&main).unwrap();
            let real_gitdir = git2::Repository::init(&main).unwrap().path().to_path_buf();
            let real_gitdir = dunce::canonicalize(&real_gitdir).unwrap_or(real_gitdir);

            let linked = temp.path().join("linked");
            fs::create_dir_all(&linked).unwrap();
            fs::write(
                linked.join(".git"),
                format!("gitdir: {}\n", real_gitdir.display()),
            )
            .unwrap();

            let resolved = find_git_dir(&linked).expect("legit gitlink must resolve");
            assert_eq!(
                resolved, real_gitdir,
                "gitlink must resolve to the real gitdir"
            );
        }

        #[test]
        fn find_sl_dir_discovers_real_repo_and_from_subdir() {
            let temp = TempDir::new().unwrap();
            let root = dunce::canonicalize(temp.path()).unwrap();
            fs::create_dir(root.join(".sl")).unwrap();

            let sd = find_sl_dir(&root).expect("repo .sl found");
            assert!(
                sd.file_name().is_some_and(|n| n == ".sl") && sd.is_dir(),
                "got {sd:?}"
            );

            let sub = root.join("crates/inner");
            fs::create_dir_all(&sub).unwrap();
            let sd_sub = find_sl_dir(&sub).expect(".sl found from subdir");
            assert_eq!(sd_sub, sd, "subdir walk must find the ancestor .sl");
        }

        #[test]
        fn find_sl_dir_none_when_no_repo() {
            // Hermetic: no `.sl` created, so none must be found inside our tree.
            let temp = TempDir::new().unwrap();
            let root = dunce::canonicalize(temp.path()).unwrap();
            let deep = root.join("no/sl/here");
            fs::create_dir_all(&deep).unwrap();
            assert!(
                find_sl_dir(&deep).is_none_or(|p| !p.starts_with(&root)),
                "must not find a .sl in a tree that contains none"
            );
        }

        #[cfg(unix)]
        #[test]
        fn find_sl_dir_rejects_symlinked_sl_to_external_dir() {
            // A `.sl` SYMLINK to an external dir must not be followed/watched:
            // the dir branch is gated on a real (non-symlink) dir.
            let external = TempDir::new().unwrap();
            let proj = TempDir::new().unwrap();
            std::os::unix::fs::symlink(external.path(), proj.path().join(".sl")).unwrap();

            let resolved = find_sl_dir(proj.path());
            let external_canon = dunce::canonicalize(external.path()).unwrap();
            assert!(
                resolved.as_deref() != Some(external_canon.as_path()),
                "symlinked .sl to an external dir must not be watched, got {resolved:?}"
            );
        }

        #[test]
        fn should_watch_separate_vcs_dir_cases() {
            let watch = Path::new("/repo/crates/codegen");
            let internal = Path::new("/repo/crates/codegen/.sl");
            let external = Path::new("/repo/.sl");
            // Fan-out: the root is non-recursive, so always watch separately.
            assert!(should_watch_separate_vcs_dir(true, internal, watch));
            assert!(should_watch_separate_vcs_dir(true, external, watch));
            // Recursive root: an internal dir is already covered — must NOT be
            // re-watched (the double-watch the design warns against)...
            assert!(!should_watch_separate_vcs_dir(false, internal, watch));
            // ...but an external ancestor (subdir cwd) must still be watched, or
            // suppression silently breaks.
            assert!(should_watch_separate_vcs_dir(false, external, watch));
        }

        #[test]
        fn external_ancestor_sl_arms_in_recursive_root_mode() {
            // Subdir cwd whose `.sl` lives in an ancestor *outside* watch_path
            // (e.g. `grok` run in `crates/codegen`): the production guard must
            // still attach the watch under a recursive root (fanout=false).
            let temp = TempDir::new().unwrap();
            let repo = dunce::canonicalize(temp.path()).unwrap();
            fs::create_dir(repo.join(".sl")).unwrap();
            let watch_path = repo.join("crates/codegen");
            fs::create_dir_all(&watch_path).unwrap();

            let sd = find_sl_dir(&watch_path).expect("ancestor .sl discovered");
            assert!(should_watch_separate_vcs_dir(false, &sd, &watch_path));
        }

        #[test]
        #[serial_test::serial]
        fn sapling_enabled_respects_kill_switch() {
            // Clear the env var even if an assert panics mid-test.
            struct Restore;
            impl Drop for Restore {
                fn drop(&mut self) {
                    // Safety: serialized test; no concurrent env access.
                    unsafe { std::env::remove_var("GROK_FSNOTIFY_SAPLING") };
                }
            }
            let _restore = Restore;

            unsafe { std::env::remove_var("GROK_FSNOTIFY_SAPLING") };
            assert!(sapling_enabled(), "default (unset) is enabled");
            for off in ["0", "false"] {
                unsafe { std::env::set_var("GROK_FSNOTIFY_SAPLING", off) };
                assert!(!sapling_enabled(), "{off:?} must disable Sapling");
            }
            unsafe { std::env::set_var("GROK_FSNOTIFY_SAPLING", "1") };
            assert!(sapling_enabled(), "any other value stays enabled");
        }

        #[test]
        fn select_capped_boundary_is_inclusive() {
            let temp = TempDir::new().unwrap();
            let root = temp.path();
            for i in 0..3 {
                fs::create_dir_all(root.join(format!("d{i}"))).unwrap();
            }
            // Exactly `cap` non-ignored dirs must fan out (inclusive `<=`); this
            // pins the edge so flipping the comparison to `<` would fail.
            assert_eq!(
                select_top_level_watch_dirs_capped(root, &None, &None, 3).map(|v| v.len()),
                Some(3),
                "count == cap must fan out"
            );
            // One past the cap falls back.
            assert!(
                select_top_level_watch_dirs_capped(root, &None, &None, 2).is_none(),
                "count > cap must fall back"
            );
            // And comfortably within the cap.
            assert_eq!(
                select_top_level_watch_dirs_capped(root, &None, &None, 4).map(|v| v.len()),
                Some(3)
            );
        }

        #[test]
        fn select_capped_does_not_count_ignored_toward_cap() {
            // Gitignored top-level dirs must not push a repo over the cap.
            let temp = TempDir::new().unwrap();
            let root = temp.path();
            fs::create_dir_all(root.join(".git")).unwrap();
            fs::write(root.join(".gitignore"), "ignored_*/\n").unwrap();
            fs::create_dir_all(root.join("real_0")).unwrap();
            fs::create_dir_all(root.join("real_1")).unwrap();
            for i in 0..5 {
                fs::create_dir_all(root.join(format!("ignored_{i}"))).unwrap();
            }
            // 7 total dirs, 5 ignored: with a cap of 2 the 2 non-ignored fit.
            let result = select_top_level_watch_dirs_capped(root, &None, &None, 2);
            assert_eq!(
                result.map(|v| v.len()),
                Some(2),
                "ignored dirs must not count toward the fan-out cap"
            );
        }

        #[test]
        fn diff_watches_add_remove_noop_combined() {
            let p = |s: &str| PathBuf::from(s);
            let set = |items: &[&str]| items.iter().map(|s| p(s)).collect::<HashSet<_>>();
            let sorted = |mut v: Vec<PathBuf>| {
                v.sort();
                v
            };

            // Add only.
            let (add, rem) = diff_watches(&set(&["/a", "/b"]), &set(&[]));
            assert_eq!(sorted(add), vec![p("/a"), p("/b")]);
            assert!(rem.is_empty());

            // Remove only.
            let (add, rem) = diff_watches(&set(&[]), &set(&["/a"]));
            assert!(add.is_empty());
            assert_eq!(rem, vec![p("/a")]);

            // No-op.
            let (add, rem) = diff_watches(&set(&["/a"]), &set(&["/a"]));
            assert!(add.is_empty() && rem.is_empty());

            // Combined.
            let (add, rem) = diff_watches(&set(&["/a", "/b"]), &set(&["/b", "/c"]));
            assert_eq!(sorted(add), vec![p("/a")]);
            assert_eq!(rem, vec![p("/c")]);
        }
    }
}
