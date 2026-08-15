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

use crate::checkout::is_another_workspace;
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
        // `git worktree add` writes the marker last, so a directory can become
        // another workspace after selection accepted it.
        if (dir_named(p, ".git") || dir_named(p, ".sl"))
            && let Some(parent) = p.parent()
            && should_skip(parent)
        {
            pruned.push(parent.to_path_buf());
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

/// What selection refuses to descend into. Never asked about the session
/// root, which is a checkout by definition.
fn should_skip(dir: &Path) -> bool {
    dir_named(dir, ".git") || dir_named(dir, ".sl") || is_another_workspace(dir)
}

/// [`should_skip`] applied to `dir` and to every directory between it and
/// `root`. `git worktree add` writes its marker last, so a directory can be
/// queued as an add before the checkout above it is recognizable.
fn should_skip_below(root: &Path, dir: &Path) -> bool {
    dir.ancestors()
        .take_while(|d| *d != root && d.starts_with(root))
        .any(should_skip)
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
        // Fan-out arms a recursive child watch, so there is no deeper prune.
        if should_skip(path) {
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

/// Ignore-aware walker that also prunes descent into anything
/// [`should_skip`] refuses. Never follows symlinks.
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
        if should_skip(path) {
            return false;
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
    watch_root: &Path,
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
    if should_skip_below(watch_root, subtree_root) {
        tracing::debug!("fs_notify: not descending into {subtree_root:?}");
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
                                            &watch_path,
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
#[path = "watcher_tests.rs"]
mod tests;

