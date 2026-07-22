use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer_opt};
use tokio::sync::mpsc;

const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(1000);

/// A [`notify::Watcher`] that drops `EventKind::Access` before it reaches the
/// debouncer, breaking the MCP/skills reload storm.
///
/// `notify`'s inotify backend emits an `Access` event on every *read*, and the
/// leader re-reads the files it watches on each reload — so unfiltered, a
/// reload's own reads schedule the next reload, a ~1/sec self-sustaining loop.
/// Dropping `Access` is safe: writes still emit `Modify`/`Create` and chmod
/// emits `Modify(Metadata)`; only reads are `Access`-only.
pub struct AccessFilteredWatcher(notify::RecommendedWatcher);

impl notify::Watcher for AccessFilteredWatcher {
    fn new<F: notify::EventHandler>(
        mut event_handler: F,
        config: notify::Config,
    ) -> notify::Result<Self>
    where
        Self: Sized,
    {
        let inner = notify::RecommendedWatcher::new(
            move |res: notify::Result<notify::Event>| match &res {
                Ok(event) if matches!(event.kind, notify::EventKind::Access(_)) => {}
                _ => event_handler.handle_event(res),
            },
            config,
        )?;
        Ok(Self(inner))
    }

    fn watch(&mut self, path: &Path, recursive_mode: RecursiveMode) -> notify::Result<()> {
        self.0.watch(path, recursive_mode)
    }

    fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        self.0.unwatch(path)
    }

    fn configure(&mut self, option: notify::Config) -> notify::Result<bool> {
        self.0.configure(option)
    }

    fn kind() -> notify::WatcherKind
    where
        Self: Sized,
    {
        notify::RecommendedWatcher::kind()
    }
}

/// `new_debouncer` equivalent that builds the debouncer on top of
/// [`AccessFilteredWatcher`] instead of the raw `RecommendedWatcher`.
fn new_filtered_debouncer<F: notify_debouncer_mini::DebounceEventHandler>(
    timeout: Duration,
    event_handler: F,
) -> Result<Debouncer<AccessFilteredWatcher>, notify::Error> {
    let config = notify_debouncer_mini::Config::default().with_timeout(timeout);
    new_debouncer_opt::<F, AccessFilteredWatcher>(config, event_handler)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigChangeEvent {
    AuthChanged,
    GlobalConfigChanged,
    /// `~/.grok/models_cache.json` changed — the on-disk `/v1/models`
    /// catalog cache was rewritten, possibly by **another** grok process
    /// sharing the same `~/.grok` (the writer may also be this process;
    /// the [`ModelsManager`](crate::agent::models::ModelsManager) dedupes
    /// by content before applying).
    ModelsCacheChanged,
    ProjectConfigChanged {
        path: PathBuf,
    },
    /// A project-scoped MCP config file changed
    /// (`<cwd>/.mcp.json` or `<cwd>/.claude.json` where `<cwd>` is a
    /// project root, **not** `$HOME`). Project `<cwd>` is derived
    /// from `path.parent()` by the reloader.
    McpConfigChanged {
        path: PathBuf,
    },
    /// The user's **home-level** `~/.claude.json` changed. Distinct
    /// from [`Self::McpConfigChanged`] because `~/.claude.json` is
    /// loaded for **every** session regardless of cwd (see
    /// `load_claude_json_mcp_servers_as_configs`), so the reload
    /// must broadcast through the legacy unit
    /// [`super::reloader::ConfigUpdate::McpServersChanged`] arm —
    /// routing it through `ProjectMcpServersChanged { cwd: $HOME }`
    /// would silently skip sessions whose cwd doesn't sit under
    /// `$HOME`.
    HomeClaudeJsonChanged,
}

/// Watches `~/.grok/` for `auth.json`, `config.toml`, and `models_cache.json`
/// changes, plus any extra paths (project `.grok/config.toml`, `.mcp.json`,
/// etc.) provided at startup.
///
/// Uses `notify-debouncer-mini` for built-in debounce that coalesces rapid
/// editor writes (including write-then-rename patterns).
///
/// Self-write suppression is intentionally omitted. When the agent writes
/// `auth.json` or `config.toml`, the watcher will fire and the
/// [`ConfigReloader`](super::reloader::ConfigReloader) will re-read the file.
/// The reloader's own content-based deduplication (auth key hash, toml value
/// comparison) skips the update when nothing actually changed, so the
/// redundant read is harmless. This avoids a class of bugs where an
/// optimistic suppression window accidentally swallows writes from external
/// processes (e.g. `grok login` in another terminal).
///
/// Adds two **non-recursive** watches per `cwd` argument:
/// `<cwd>/` (catches `.mcp.json` and `.claude.json` at the project root) and
/// `<cwd>/.grok/` (catches `<cwd>/.grok/config.toml`). Recursing on `<cwd>`
/// would walk `node_modules/`, `target/`, `.git/`, etc. and blow through
/// `fs.inotify.max_user_watches` on large repos. Use [`Self::watch_path`]
/// to register additional cwds at runtime when new sessions open in
/// previously-unwatched directories.
pub struct ConfigFileWatcher {
    debouncer: Debouncer<AccessFilteredWatcher>,
    /// Project cwds currently registered (via [`Self::start`]'s `cwd`
    /// argument or [`Self::watch_path`]). Tracked so that
    /// (a) [`Self::watch_path`] is idempotent at our layer instead of
    /// relying on `notify`'s internal de-dup, and
    /// (b) [`Self::unwatch_path`] can drop the OS watches for a cwd
    /// that is no longer needed, bounding inotify-watch accumulation
    /// as sessions churn across directories.
    watched_cwds: HashSet<PathBuf>,
}

impl ConfigFileWatcher {
    /// Start watching. Returns `None` if the OS watcher fails to initialize.
    ///
    /// `cwd`, when `Some`, adds two non-recursive watches: `<cwd>/` and
    /// `<cwd>/.grok/`. Use [`Self::watch_path`] later to register additional
    /// project cwds for sessions that open in previously-unwatched
    /// directories.
    pub fn start(
        grok_home: &Path,
        extra_paths: &[PathBuf],
        cwd: Option<&Path>,
        debounce: Option<Duration>,
    ) -> Option<(Self, mpsc::UnboundedReceiver<ConfigChangeEvent>)> {
        let debounce = debounce.unwrap_or(DEFAULT_DEBOUNCE);
        let (tx, rx) = mpsc::unbounded_channel();
        let grok_home_buf = grok_home.to_path_buf();
        // `~/.claude.json` is consumed by **every**
        // session (see `load_claude_json_mcp_servers_as_configs`), so
        // a write to it must broadcast through the unit
        // `McpServersChanged` arm — NOT through the per-cwd
        // `ProjectMcpServersChanged { cwd: $HOME }` arm, which
        // `cwd_matches` would silently filter for sessions outside
        // `$HOME`. We snapshot `$HOME` here so the closure can
        // discriminate `<home>/.claude.json` from a project-level
        // `<cwd>/.claude.json` purely by path.
        //
        // Canonicalize `$HOME` ONCE up front. `notify`
        // backends may deliver canonicalized event paths (e.g. macOS
        // FSEvents resolves symlinks, returning `/private/var/...`
        // where `dirs::home_dir()` returned `/var/...`), so a raw byte
        // compare against an un-canonicalized `$HOME` would mis-route
        // `~/.claude.json` to the per-cwd path. The per-event side is
        // canonicalized in `parent_is_dir`.
        let user_home_buf: Option<PathBuf> =
            dirs::home_dir().map(|h| dunce::canonicalize(&h).unwrap_or(h));

        let mut debouncer = new_filtered_debouncer(debounce, move |res: DebounceEventResult| {
            let Ok(events) = res else { return };

            let mut batch_events: Vec<ConfigChangeEvent> = Vec::new();
            for event in events {
                let path = &event.path;
                let name = path.file_name().and_then(|n| n.to_str());
                let parent = path.parent();

                let change = match name {
                    Some("auth.json") if parent == Some(grok_home_buf.as_path()) => {
                        Some(ConfigChangeEvent::AuthChanged)
                    }
                    Some("config.toml") if parent == Some(grok_home_buf.as_path()) => {
                        Some(ConfigChangeEvent::GlobalConfigChanged)
                    }
                    Some("models_cache.json") if parent == Some(grok_home_buf.as_path()) => {
                        Some(ConfigChangeEvent::ModelsCacheChanged)
                    }
                    Some("config.toml") => {
                        Some(ConfigChangeEvent::ProjectConfigChanged { path: path.clone() })
                    }
                    // `~/.claude.json` routes through
                    // the dedicated home-level variant so the
                    // reloader can broadcast. Project-level
                    // `<cwd>/.claude.json` (and any `.mcp.json`)
                    // continues to be a per-cwd reload.
                    Some(".claude.json")
                        if user_home_buf
                            .as_deref()
                            .is_some_and(|h| parent_is_dir(parent, h)) =>
                    {
                        Some(ConfigChangeEvent::HomeClaudeJsonChanged)
                    }
                    Some(".mcp.json") | Some(".claude.json") => {
                        Some(ConfigChangeEvent::McpConfigChanged { path: path.clone() })
                    }
                    _ => None,
                };

                if let Some(evt) = change
                    && !batch_events.contains(&evt)
                {
                    batch_events.push(evt);
                }
            }
            for evt in batch_events {
                let _ = tx.send(evt);
            }
        })
        .map_err(|e| tracing::warn!(error = %e, "failed to create config file watcher"))
        .ok()?;

        debouncer
            .watcher()
            .watch(grok_home, RecursiveMode::NonRecursive)
            .map_err(|e| {
                tracing::warn!(
                    path = %grok_home.display(),
                    error = %e,
                    "failed to watch grok home directory"
                )
            })
            .ok()?;

        for p in extra_paths {
            if let Some(parent) = p.parent() {
                let _ = debouncer
                    .watcher()
                    .watch(parent, RecursiveMode::NonRecursive);
            }
        }

        // Add the two narrow non-recursive cwd watches
        // promoted to first-class watch targets. Both are non-fatal —
        // a missing directory just means the corresponding files don't
        // exist yet and will be picked up by `watch_path` on the next
        // session that opens in this cwd.
        //
        // When the leader's own cwd is also covered by
        // `extra_paths` (e.g. `find_project_configs(cwd)` already
        // includes `<cwd>/.grok/config.toml` so the loop above
        // watches `<cwd>/.grok/`), the call below installs a
        // duplicate watch on the same directory. `notify` dedupes
        // silently in its `RecommendedWatcher` (last-write-wins for
        // the recursion mode), so this is cosmetic — both
        // additions remain non-recursive, no event amplification.
        let mut watched_cwds = HashSet::new();
        if let Some(cwd) = cwd {
            watch_cwd_dirs(&mut debouncer, cwd);
            watched_cwds.insert(cwd.to_path_buf());
        }

        tracing::info!(
            grok_home = %grok_home.display(),
            extra_paths = extra_paths.len(),
            cwd = ?cwd,
            debounce_ms = debounce.as_millis(),
            "config file watcher started"
        );

        Some((
            Self {
                debouncer,
                watched_cwds,
            },
            rx,
        ))
    }

    /// Register `<cwd>/` and `<cwd>/.grok/` as **non-recursive** watch
    /// targets, in addition to whatever was passed to [`Self::start`].
    ///
    /// Intended for the session-open path: when a session opens in a cwd
    /// the leader hasn't seen before, calling this method ensures edits to
    /// `<cwd>/.mcp.json` and `<cwd>/.grok/config.toml` trigger a
    /// [`ConfigChangeEvent`] (and downstream [`ConfigUpdate::
    /// ProjectMcpServersChanged`](super::reloader::ConfigUpdate::
    /// ProjectMcpServersChanged)) within the debounce window.
    ///
    /// **Non-recursive by design.** Watching `<cwd>` recursively would
    /// walk `node_modules/`, `target/`, `.git/`, etc. and easily exhaust
    /// the per-user inotify quota (`fs.inotify.max_user_watches`,
    /// commonly 8192 by default) on a large repo. If `notify` cannot register the watch (e.g.
    /// the directory doesn't exist yet, or the OS quota is reached) the
    /// error is logged and swallowed — the leader continues to rely on
    /// the user-triggered refresh as the fallback.
    pub fn watch_path(&mut self, cwd: &Path) {
        // Idempotent at our layer: skip the redundant
        // `notify` watch-add when this cwd is already registered, so
        // re-opening sessions in the same directory doesn't churn the
        // OS watcher. `notify` de-dups internally too, but tracking the
        // set here also enables `unwatch_path`.
        if self.watched_cwds.contains(cwd) {
            return;
        }
        watch_cwd_dirs(&mut self.debouncer, cwd);
        self.watched_cwds.insert(cwd.to_path_buf());
    }

    /// Remove the two non-recursive watches (`<cwd>/` and
    /// `<cwd>/.grok/`) previously registered for `cwd` via
    /// [`Self::start`] / [`Self::watch_path`].
    ///
    /// Best-effort and idempotent: a `cwd` that was never registered
    /// (or already unwatched) is a no-op. Intended for the
    /// session-teardown path so a long-lived leader that opens sessions
    /// across many directories doesn't accumulate inotify watches for
    /// cwds with no live sessions. **Callers must ref-count**: only
    /// unwatch once the *last* session sharing this cwd closes —
    /// `ConfigFileWatcher` tracks distinct cwds, not session counts.
    pub fn unwatch_path(&mut self, cwd: &Path) {
        if !self.watched_cwds.remove(cwd) {
            return;
        }
        unwatch_cwd_dirs(&mut self.debouncer, cwd);
    }
}

/// Component-aware "is `parent` the directory `dir`?" that tolerates
/// symlink / canonicalization differences between a `notify`-delivered
/// event path and a `dirs::home_dir()`-style reference. `dir` is
/// expected to be already canonicalized (see `ConfigFileWatcher::start`).
fn parent_is_dir(parent: Option<&Path>, dir: &Path) -> bool {
    let Some(parent) = parent else {
        return false;
    };
    parent == dir || dunce::canonicalize(parent).is_ok_and(|p| p == dir)
}

/// Add the two non-recursive watches for a project root.
///
/// Both watches are best-effort and log-and-continue on failure (missing
/// directory, quota exhausted, permission denied, etc.) — the caller has
/// no reasonable recovery path beyond the existing user-triggered refresh.
///
/// **Known limitation:** if `<cwd>/.grok/` does not yet
/// exist at session-open time, the `.grok/` watch fails ENOENT and is
/// swallowed at `debug!`. A later `mkdir <cwd>/.grok/` followed by a
/// write to `<cwd>/.grok/config.toml` will NOT be observed — the
/// `<cwd>/` watch is non-recursive, so subdirectory creation isn't
/// surfaced as a watch-add trigger. Users hitting this case must hit
/// the explicit refresh button. A robust fix (re-attempt on parent-
/// directory create) is out of scope here.
fn watch_cwd_dirs(debouncer: &mut Debouncer<AccessFilteredWatcher>, cwd: &Path) {
    if let Err(e) = debouncer.watcher().watch(cwd, RecursiveMode::NonRecursive) {
        log_watch_error(&e, "failed to watch project cwd (non-recursive)");
    }
    let grok_dir = cwd.join(".grok");
    if let Err(e) = debouncer
        .watcher()
        .watch(&grok_dir, RecursiveMode::NonRecursive)
    {
        log_watch_error(
            &e,
            "failed to watch project .grok directory (non-recursive)",
        );
    }
}

/// Remove the two non-recursive watches added by [`watch_cwd_dirs`].
/// Best-effort: a `WatchNotFound` (never watched / already removed) is
/// expected and logged at `debug!`.
fn unwatch_cwd_dirs(debouncer: &mut Debouncer<AccessFilteredWatcher>, cwd: &Path) {
    if let Err(e) = debouncer.watcher().unwatch(cwd) {
        tracing::debug!(error = %e, "failed to unwatch project cwd");
    }
    let grok_dir = cwd.join(".grok");
    if let Err(e) = debouncer.watcher().unwatch(&grok_dir) {
        tracing::debug!(error = %e, "failed to unwatch project .grok directory");
    }
}

/// Log a `notify` watch failure, distinguishing the benign
/// "directory doesn't exist yet" case (logged at `debug!` — it's
/// expected for a freshly-opened session whose `<cwd>/.grok/` hasn't
/// been created) from genuinely actionable failures like
/// `fs.inotify.max_user_watches` exhaustion or permission denied
/// (logged at `warn!` — these mean live edits will be silently
/// missed). Don't swallow every error at the same level.
fn log_watch_error(err: &notify::Error, msg: &str) {
    let not_found = matches!(err.kind, notify::ErrorKind::PathNotFound)
        || matches!(&err.kind, notify::ErrorKind::Io(io) if io.kind() == std::io::ErrorKind::NotFound);
    if not_found {
        tracing::debug!(error = %err, "{msg} (path not found)");
    } else {
        tracing::warn!(error = %err, "{msg}");
    }
}

pub struct SkillsFileWatcher {
    debouncer: Debouncer<AccessFilteredWatcher>,
    refresh_dirs: Vec<(PathBuf, RecursiveMode)>,
    refreshed_dirs: HashSet<PathBuf>,
}

const SKILLS_DEBOUNCE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryChange {
    Skills,
    Workflows,
}

fn discovery_change_for_path(path: &Path) -> Option<DiscoveryChange> {
    if path.file_name().is_some_and(|name| name == ".grok") {
        return Some(DiscoveryChange::Skills);
    }
    if path.file_name().is_some_and(|name| name == "workflows")
        || path
            .ancestors()
            .any(|ancestor| ancestor.file_name().is_some_and(|name| name == "workflows"))
    {
        return Some(DiscoveryChange::Workflows);
    }
    if path.file_name().is_some_and(|name| name == "SKILL.md")
        || path
            .ancestors()
            .any(|ancestor| ancestor.file_name().is_some_and(|name| name == "skills"))
        || (path.extension().is_some_and(|extension| extension == "md")
            && path
                .parent()
                .is_some_and(|parent| parent.file_name().is_some_and(|name| name == "commands")))
    {
        return Some(DiscoveryChange::Skills);
    }
    None
}

/// True for a global/home-level config dir that must never be watched
/// recursively: `grok_home` (`~/.grok`, or `$GROK_HOME`) or a known vendor dir
/// directly under `$HOME` ([`HOME_VENDOR_DIRS`]).
///
/// These hold large non-skill trees — `~/.grok` alone has `worktrees/`,
/// `sessions/`, `logs/`, `upload_queue/` — so recursing them exhausted the
/// inotify quota (~780k watches on a devbox) and, since each worktree is a full
/// checkout, fired skill reloads on ordinary repo activity. They get scoped
/// watches instead ([`watch_skill_subdirs`]); project/repo dirs — and
/// user-supplied `[skills].paths` entries, which discovery walks in full — stay
/// recursive. Matching only these specific names (not "any dir whose parent is
/// `$HOME`") is what keeps a `[skills].paths = ["~/my-skills"]` fully watched.
fn is_global_config_dir(dir: &Path, grok_home: &Path) -> bool {
    #[allow(deprecated)]
    let home = std::env::home_dir();
    is_global_config_dir_impl(dir, grok_home, home.as_deref())
}

/// Vendor config dir names that sit directly under `$HOME` and carry large
/// non-skill trees. Kept in sync with the home-level dirs added by
/// `collect_skill_config_dirs`.
const HOME_VENDOR_DIRS: &[&str] = &[".grok", ".agents", ".claude", ".cursor"];

/// Testable core of [`is_global_config_dir`] with `$HOME` injected.
fn is_global_config_dir_impl(dir: &Path, grok_home: &Path, home: Option<&Path>) -> bool {
    let canon = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if canon(dir) == canon(grok_home) {
        return true;
    }
    let Some(home) = home else { return false };
    if dir.parent().map(canon) != Some(canon(home)) {
        return false;
    }
    dir.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| HOME_VENDOR_DIRS.contains(&n))
}

fn watch_skill_subdirs(
    debouncer: &mut Debouncer<AccessFilteredWatcher>,
    config_dir: &Path,
) -> usize {
    let mut watched = 0;
    match debouncer
        .watcher()
        .watch(config_dir, RecursiveMode::NonRecursive)
    {
        Ok(()) => watched += 1,
        Err(error) => log_watch_error(&error, "failed to watch config dir root"),
    }
    for (subdir, mode) in [
        ("skills", RecursiveMode::Recursive),
        ("commands", RecursiveMode::NonRecursive),
        ("workflows", RecursiveMode::NonRecursive),
    ] {
        let dir = config_dir.join(subdir);
        if dir.is_dir() {
            match debouncer.watcher().watch(&dir, mode) {
                Ok(()) => watched += 1,
                Err(error) => log_watch_error(&error, "failed to watch discovery subdir"),
            }
        }
    }
    watched
}

pub struct ProjectDiscoveryWatcher {
    debouncer: Debouncer<AccessFilteredWatcher>,
    refresh_dirs: Vec<(PathBuf, RecursiveMode)>,
    refreshed_dirs: HashSet<PathBuf>,
}

impl ProjectDiscoveryWatcher {
    pub fn start(cwd: &Path) -> Option<(Self, mpsc::UnboundedReceiver<DiscoveryChange>)> {
        let project_root = crate::session::workflow::registry::project_root(cwd);
        let project_grok = project_root.join(".grok");
        let workflows = project_grok.join("workflows");
        let (tx, rx) = mpsc::unbounded_channel();
        let project_grok_for_events = project_grok.clone();
        let mut debouncer =
            new_filtered_debouncer(SKILLS_DEBOUNCE, move |res: DebounceEventResult| {
                let Ok(events) = res else { return };
                let mut change = None;
                for event in events
                    .iter()
                    .filter(|event| event.path.starts_with(&project_grok_for_events))
                {
                    let next = discovery_change_for_path(&event.path)
                        .unwrap_or(DiscoveryChange::Workflows);
                    if next == DiscoveryChange::Skills {
                        change = Some(next);
                        break;
                    }
                    change = Some(next);
                }
                if let Some(change) = change {
                    let _ = tx.send(change);
                }
            })
            .map_err(|error| tracing::warn!(%error, "failed to create project workflow watcher"))
            .ok()?;

        let initial = if project_grok.is_dir() {
            project_grok.clone()
        } else {
            project_root.clone()
        };
        if let Err(error) = debouncer
            .watcher()
            .watch(&initial, RecursiveMode::NonRecursive)
        {
            log_watch_error(&error, "failed to watch project workflow parent");
            return None;
        }
        let refresh_dirs = vec![
            (project_grok, RecursiveMode::NonRecursive),
            (
                project_root.join(".grok").join("skills"),
                RecursiveMode::Recursive,
            ),
            (
                project_root.join(".grok").join("commands"),
                RecursiveMode::NonRecursive,
            ),
            (workflows, RecursiveMode::NonRecursive),
        ];
        let mut refreshed_dirs = HashSet::from([initial]);
        for (dir, mode) in &refresh_dirs {
            if refreshed_dirs.contains(dir) || !dir.is_dir() {
                continue;
            }
            match debouncer.watcher().watch(dir, *mode) {
                Ok(()) => {
                    refreshed_dirs.insert(dir.clone());
                }
                Err(error) => log_watch_error(&error, "failed to watch project discovery dir"),
            }
        }
        Some((
            Self {
                debouncer,
                refresh_dirs,
                refreshed_dirs,
            },
            rx,
        ))
    }

    pub fn refresh_new_dirs(&mut self) {
        for (dir, mode) in &self.refresh_dirs {
            if self.refreshed_dirs.contains(dir) || !dir.is_dir() {
                continue;
            }
            match self.debouncer.watcher().watch(dir, *mode) {
                Ok(()) => {
                    self.refreshed_dirs.insert(dir.clone());
                }
                Err(error) => {
                    log_watch_error(&error, "failed to watch newly-created project workflow dir")
                }
            }
        }
    }
}

impl SkillsFileWatcher {
    ///
    /// Uses [`collect_skill_config_dirs`](xai_grok_agent::prompt::skills::collect_skill_config_dirs)
    /// as the canonical directory source so the watcher covers the same
    /// locations as skill discovery.
    pub fn start(
        cwd: Option<&Path>,
        monorepo_user_dir: Option<&Path>,
        config_paths: &[String],
    ) -> Option<(Self, mpsc::UnboundedReceiver<DiscoveryChange>)> {
        let (tx, rx) = mpsc::unbounded_channel();

        let mut debouncer =
            new_filtered_debouncer(SKILLS_DEBOUNCE, move |res: DebounceEventResult| {
                let Ok(events) = res else { return };
                let mut change = None;
                for next in events
                    .iter()
                    .filter_map(|event| discovery_change_for_path(&event.path))
                {
                    if next == DiscoveryChange::Skills {
                        change = Some(next);
                        break;
                    }
                    change = Some(next);
                }
                if let Some(change) = change {
                    let _ = tx.send(change);
                }
            })
            .map_err(|e| tracing::warn!(error = %e, "failed to create skills file watcher"))
            .ok()?;

        let grok_home = xai_grok_tools::util::grok_home::grok_home();
        // Watch the full superset of vendor dirs (all-on compat). This watcher
        // is leader-global (no per-session compat resolved here); the actual
        // per-session discovery gating happens downstream, so watching a
        // currently-disabled vendor dir is harmless (a change just re-runs the
        // gated discovery) and avoids ever missing a watch if a toggle flips.
        let dirs_to_watch = xai_grok_agent::prompt::skills::collect_skill_config_dirs(
            cwd,
            monorepo_user_dir,
            &grok_home,
            config_paths,
            xai_grok_tools::types::compat::CompatConfig::default(),
        );
        let mut watched = 0;
        for dir in &dirs_to_watch {
            if is_global_config_dir(dir, &grok_home) {
                watched += watch_skill_subdirs(&mut debouncer, dir);
            } else {
                // Project/repo dir: bounded, so recurse to catch new `skills/`
                // dirs created mid-session as well as edits to existing files.
                match debouncer.watcher().watch(dir, RecursiveMode::Recursive) {
                    Ok(()) => watched += 1,
                    Err(e) => log_watch_error(&e, "failed to watch directory for skill changes"),
                }
            }
        }

        if watched == 0 {
            tracing::debug!("no config directories found to watch for skills");
            return None;
        }

        tracing::info!(dirs = watched, "skills file watcher started");

        let mut refresh_dirs = vec![(grok_home.join("workflows"), RecursiveMode::NonRecursive)];
        if let Some(cwd) = cwd {
            let project_root = crate::session::workflow::registry::project_root(cwd);
            let project_grok = project_root.join(".grok");
            let parent_watch = if project_grok.is_dir() {
                project_grok.clone()
            } else {
                project_root
            };
            if !dirs_to_watch.iter().any(|dir| dir == &parent_watch) {
                match debouncer
                    .watcher()
                    .watch(&parent_watch, RecursiveMode::NonRecursive)
                {
                    Ok(()) => {}
                    Err(error) => log_watch_error(
                        &error,
                        "failed to watch workflow discovery parent directory",
                    ),
                }
            }
            refresh_dirs.push((project_grok.clone(), RecursiveMode::NonRecursive));
            refresh_dirs.push((project_grok.join("workflows"), RecursiveMode::NonRecursive));
        }
        let refreshed_dirs = refresh_dirs
            .iter()
            .filter(|(dir, _)| dir.is_dir())
            .map(|(dir, _)| dir.clone())
            .collect();
        Some((
            Self {
                debouncer,
                refresh_dirs,
                refreshed_dirs,
            },
            rx,
        ))
    }

    pub fn refresh_new_discovery_dirs(&mut self) -> bool {
        let mut changed = false;
        for (dir, mode) in &self.refresh_dirs {
            if self.refreshed_dirs.contains(dir) || !dir.is_dir() {
                continue;
            }
            match self.debouncer.watcher().watch(dir, *mode) {
                Ok(()) => {
                    self.refreshed_dirs.insert(dir.clone());
                    changed = true;
                }
                Err(error) => {
                    log_watch_error(&error, "failed to watch newly-created discovery directory")
                }
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn wait_ms(ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }

    /// `is_global_config_dir` must scope down only grok_home and the known
    /// vendor dirs under `$HOME` — NOT arbitrary `[skills].paths` entries such
    /// as `~/my-skills`, whose skills discovery walks in full and so must stay
    /// recursively watched.
    #[test]
    fn is_global_config_dir_matches_only_grok_home_and_vendor_dirs() {
        let home = TempDir::new().unwrap();
        let home = home.path();
        let grok_home = home.join(".grok");

        let g = |dir: &Path| is_global_config_dir_impl(dir, &grok_home, Some(home));

        // grok_home and vendor dirs directly under $HOME: scoped (global).
        assert!(g(&grok_home));
        assert!(g(&home.join(".claude")));
        assert!(g(&home.join(".cursor")));
        assert!(g(&home.join(".agents")));

        // A user [skills].paths entry under $HOME: NOT global (stays recursive).
        assert!(!g(&home.join("my-skills")));
        assert!(!g(&home.join(".config")));
        // A project/repo config dir (parent isn't $HOME): NOT global.
        assert!(!g(&home.join("repo").join(".grok")));
    }

    /// Regression for the ~/.grok inotify-exhaustion / worktree-noise bug: a
    /// `SKILL.md` under a sibling subtree (e.g. `~/.grok/worktrees/`) must not
    /// drive a reload, while a real `<dir>/skills/**/SKILL.md` change still does.
    #[test]
    #[cfg(target_os = "linux")]
    fn skills_watcher_scopes_to_subdirs_not_dir_root() {
        let tmp = TempDir::new().unwrap();
        let global = tmp.path();

        // Real global skill under <dir>/skills/.
        let alpha = global.join("skills").join("alpha");
        fs::create_dir_all(&alpha).unwrap();
        fs::write(alpha.join("SKILL.md"), "# alpha").unwrap();

        let wt_skill = global
            .join("worktrees")
            .join("wt1")
            .join(".grok")
            .join("skills")
            .join("beta");
        fs::create_dir_all(&wt_skill).unwrap();
        fs::write(wt_skill.join("SKILL.md"), "# beta").unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut debouncer = new_filtered_debouncer(
            Duration::from_millis(50),
            move |res: DebounceEventResult| {
                let Ok(events) = res else { return };
                if events
                    .iter()
                    .any(|event| discovery_change_for_path(&event.path).is_some())
                {
                    let _ = tx.send(());
                }
            },
        )
        .expect("debouncer should build");

        let watched = watch_skill_subdirs(&mut debouncer, global);
        assert!(watched >= 1, "should watch the <dir>/skills subdir");
        wait_ms(150);
        while rx.try_recv().is_ok() {} // drain startup noise

        // Editing a SKILL.md under the unwatched worktrees/ subtree must NOT fire.
        fs::write(wt_skill.join("SKILL.md"), "# beta v2").unwrap();
        wait_ms(250);
        assert!(
            rx.try_recv().is_err(),
            "changes below an unwatched sibling subtree (worktrees/) must not \
             trigger a skills reload — proves the dir root is non-recursive"
        );

        // Editing the real skill under <dir>/skills must still fire.
        fs::write(alpha.join("SKILL.md"), "# alpha v2").unwrap();
        wait_ms(250);
        assert!(
            rx.try_recv().is_ok(),
            "changes under <dir>/skills must trigger a skills reload"
        );
    }

    #[test]
    fn workflow_change_classifies_missing_directory_creation() {
        let grok = Path::new("/tmp/project/.grok");
        assert_eq!(
            discovery_change_for_path(grok),
            Some(DiscoveryChange::Skills),
            "first .grok creation must take the broader skills reload path"
        );
        assert_eq!(
            discovery_change_for_path(&grok.join("workflows")),
            Some(DiscoveryChange::Workflows)
        );
        assert_eq!(
            discovery_change_for_path(&grok.join("workflows/review.rhai")),
            Some(DiscoveryChange::Workflows)
        );
        assert_eq!(
            discovery_change_for_path(&grok.join("skills/review/SKILL.md")),
            Some(DiscoveryChange::Skills)
        );
    }

    #[test]
    fn refresh_new_discovery_dirs_attaches_first_created_workflows_dir() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let root_for_handler = root.to_path_buf();
        let mut debouncer = new_filtered_debouncer(
            Duration::from_millis(50),
            move |result: DebounceEventResult| {
                let Ok(events) = result else { return };
                if events
                    .iter()
                    .any(|event| event.path.starts_with(&root_for_handler))
                {
                    let _ = tx.send(());
                }
            },
        )
        .unwrap();
        debouncer
            .watcher()
            .watch(root, RecursiveMode::NonRecursive)
            .unwrap();
        let workflows = root.join("workflows");
        let mut watcher = SkillsFileWatcher {
            debouncer,
            refresh_dirs: vec![(workflows.clone(), RecursiveMode::NonRecursive)],
            refreshed_dirs: HashSet::new(),
        };
        fs::create_dir(&workflows).unwrap();
        wait_ms(150);
        assert!(
            rx.try_recv().is_ok(),
            "parent watch sees first directory creation"
        );
        assert!(watcher.refresh_new_discovery_dirs());
        assert!(watcher.refreshed_dirs.contains(&workflows));
    }

    #[test]
    #[cfg_attr(
        target_os = "macos",
        ignore = "flaky on macOS: FSEvents does not reliably deliver events in test harness"
    )]
    fn watcher_detects_auth_json_change() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("auth.json"), "{}").unwrap();

        let (_w, mut rx) =
            ConfigFileWatcher::start(tmp.path(), &[], None, Some(Duration::from_millis(50)))
                .expect("watcher should start");

        fs::write(tmp.path().join("auth.json"), r#"{"new":"token"}"#).unwrap();
        wait_ms(300);

        let mut found = false;
        while let Ok(evt) = rx.try_recv() {
            if evt == ConfigChangeEvent::AuthChanged {
                found = true;
            }
        }
        assert!(found, "should detect auth.json change");
    }

    /// Regression test for the MCP/skills reload storm (feedback loop):
    /// merely *reading* a watched config file must NOT produce a
    /// `ConfigChangeEvent`. Linux inotify delivers `IN_OPEN`/`IN_ACCESS`
    /// for reads, `notify` subscribes to `OPEN`, and `notify-debouncer-mini`
    /// forwards every kind — so without [`AccessFilteredWatcher`] each
    /// leader-initiated reload's own re-reads of `config.toml` would
    /// schedule the next debounce tick and re-fire forever. A write
    /// afterwards must still be detected (the filter only drops `Access`).
    #[test]
    #[cfg(target_os = "linux")]
    fn watcher_ignores_reads_of_watched_files() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("config.toml"), "a = 1").unwrap();
        fs::write(tmp.path().join("auth.json"), "{}").unwrap();

        let (_w, mut rx) =
            ConfigFileWatcher::start(tmp.path(), &[], None, Some(Duration::from_millis(50)))
                .expect("watcher should start");
        wait_ms(150);
        while rx.try_recv().is_ok() {} // drain any startup noise

        // Simulate what the leader does on every reload: read the watched
        // files. Repeatedly, to defeat any incidental coalescing.
        for _ in 0..5 {
            let _ = fs::read(tmp.path().join("config.toml")).unwrap();
            let _ = fs::read(tmp.path().join("auth.json")).unwrap();
            wait_ms(20);
        }
        wait_ms(300);

        let mut read_events = Vec::new();
        while let Ok(evt) = rx.try_recv() {
            read_events.push(evt);
        }
        assert!(
            read_events.is_empty(),
            "reads of watched files must not emit config-change events \
             (reload-storm feedback loop); got {read_events:?}"
        );

        // Sanity: a real write is still observed through the filter.
        fs::write(tmp.path().join("config.toml"), "a = 2").unwrap();
        wait_ms(300);
        let mut found = false;
        while let Ok(evt) = rx.try_recv() {
            if evt == ConfigChangeEvent::GlobalConfigChanged {
                found = true;
            }
        }
        assert!(found, "a write must still be detected after read filtering");
    }

    #[test]
    #[cfg_attr(
        target_os = "macos",
        ignore = "flaky on macOS: FSEvents does not reliably deliver events in test harness"
    )]
    fn watcher_detects_config_toml_change() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("config.toml"), "").unwrap();

        let (_w, mut rx) =
            ConfigFileWatcher::start(tmp.path(), &[], None, Some(Duration::from_millis(50)))
                .expect("watcher should start");

        fs::write(tmp.path().join("config.toml"), "[ui]\ntheme = \"dark\"").unwrap();
        wait_ms(300);

        let mut found = false;
        while let Ok(evt) = rx.try_recv() {
            if evt == ConfigChangeEvent::GlobalConfigChanged {
                found = true;
            }
        }
        assert!(found, "should detect config.toml change");
    }

    /// A write to `<grok_home>/models_cache.json` must surface as
    /// `ConfigChangeEvent::ModelsCacheChanged` so a long-running leader can
    /// hot-load a catalog fetched by another grok process.
    #[test]
    #[cfg_attr(
        target_os = "macos",
        ignore = "flaky on macOS: FSEvents does not reliably deliver events in test harness"
    )]
    fn watcher_detects_models_cache_change() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("models_cache.json"), "{}").unwrap();

        let (_w, mut rx) =
            ConfigFileWatcher::start(tmp.path(), &[], None, Some(Duration::from_millis(50)))
                .expect("watcher should start");

        fs::write(
            tmp.path().join("models_cache.json"),
            r#"{"fetched_at":"2026-01-01T00:00:00Z","models":{}}"#,
        )
        .unwrap();
        wait_ms(300);

        let mut found = false;
        while let Ok(evt) = rx.try_recv() {
            if evt == ConfigChangeEvent::ModelsCacheChanged {
                found = true;
            }
        }
        assert!(found, "should detect models_cache.json change");
    }

    #[test]
    #[ignore = "flaky on CI: OS file watcher may fail to initialize"]
    fn watcher_ignores_unrelated_files() {
        let tmp = TempDir::new().unwrap();

        let (_w, mut rx) =
            ConfigFileWatcher::start(tmp.path(), &[], None, Some(Duration::from_millis(50)))
                .expect("watcher should start");

        fs::write(tmp.path().join("leader.log"), "log line").unwrap();
        fs::write(tmp.path().join("leader.lock"), "12345").unwrap();
        wait_ms(300);

        assert!(
            rx.try_recv().is_err(),
            "should not emit events for unrelated files"
        );
    }

    #[test]
    fn watcher_debounces_rapid_writes() {
        let tmp = TempDir::new().unwrap();

        // Use a long debounce (500ms) so all rapid writes (50ms total)
        // land in a single debounce window regardless of platform.
        let (_w, mut rx) =
            ConfigFileWatcher::start(tmp.path(), &[], None, Some(Duration::from_millis(500)))
                .expect("watcher should start");

        wait_ms(200);

        // 5 rapid writes — total ~50ms, well within the 500ms debounce window
        for i in 0..5 {
            fs::write(tmp.path().join("config.toml"), format!("version = {i}")).unwrap();
            wait_ms(10);
        }
        // Wait for the single debounce tick to fire
        wait_ms(800);

        let mut count = 0;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        // All writes should coalesce into a small number of events
        // (1 per debounce tick, or a few if OS delivers events in
        // separate batches within the window).
        assert!(count >= 1, "expected at least 1 event, got {count}");
        assert!(count <= 3, "expected coalesced events (<=3), got {count}");
    }

    /// A write to `<cwd>/.grok/config.toml` must surface as
    /// a `ConfigChangeEvent::ProjectConfigChanged` so the reloader emits
    /// `ConfigUpdate::ProjectMcpServersChanged { cwd }`. Uses a longer
    /// debounce and explicit poll loop so it survives the slower-than-
    /// usual FSEvents delivery on macOS CI.
    #[test]
    #[cfg_attr(
        target_os = "macos",
        ignore = "flaky on macOS: FSEvents does not reliably deliver events in test harness"
    )]
    fn project_cwd_toml_triggers_reload() {
        let grok_home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let project_grok = cwd.path().join(".grok");
        fs::create_dir_all(&project_grok).unwrap();
        // Seed the file before the watcher starts so we observe the
        // modification rather than the creation event.
        fs::write(project_grok.join("config.toml"), "").unwrap();

        let (_w, mut rx) = ConfigFileWatcher::start(
            grok_home.path(),
            &[],
            Some(cwd.path()),
            Some(Duration::from_millis(100)),
        )
        .expect("watcher should start");

        fs::write(
            project_grok.join("config.toml"),
            "[mcp_servers.x]\ncommand = \"/bin/true\"",
        )
        .unwrap();

        // Poll up to 2s for the event.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut found = false;
        while std::time::Instant::now() < deadline {
            if let Ok(evt) = rx.try_recv()
                && matches!(evt, ConfigChangeEvent::ProjectConfigChanged { .. })
            {
                found = true;
                break;
            }
            wait_ms(50);
        }
        assert!(
            found,
            "expected ProjectConfigChanged for <cwd>/.grok/config.toml within 2s"
        );
    }

    /// A write to `<cwd>/.mcp.json` must surface as a
    /// `ConfigChangeEvent::McpConfigChanged` so the reloader can fan out
    /// a `ProjectMcpServersChanged { cwd }`. Same FSEvents caveat as
    /// [`project_cwd_toml_triggers_reload`].
    #[test]
    #[cfg_attr(
        target_os = "macos",
        ignore = "flaky on macOS: FSEvents does not reliably deliver events in test harness"
    )]
    fn project_mcp_json_triggers_reload() {
        let grok_home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        fs::write(cwd.path().join(".mcp.json"), "{}").unwrap();

        let (_w, mut rx) = ConfigFileWatcher::start(
            grok_home.path(),
            &[],
            Some(cwd.path()),
            Some(Duration::from_millis(100)),
        )
        .expect("watcher should start");

        fs::write(
            cwd.path().join(".mcp.json"),
            r#"{"mcpServers": {"x": {"command": "/bin/true"}}}"#,
        )
        .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut found = false;
        while std::time::Instant::now() < deadline {
            if let Ok(evt) = rx.try_recv()
                && matches!(evt, ConfigChangeEvent::McpConfigChanged { .. })
            {
                found = true;
                break;
            }
            wait_ms(50);
        }
        assert!(
            found,
            "expected McpConfigChanged for <cwd>/.mcp.json within 2s"
        );
    }

    /// The cwd watch is **non-recursive** by design. This writes a
    /// file that the watcher's name filter **would** route
    /// (`.mcp.json`) into a deeply nested subdir. If a future
    /// regression flips `RecursiveMode::NonRecursive` → `Recursive`,
    /// recursive notify would surface the write, the name filter would
    /// map it to `McpConfigChanged`, and the test would fail. The file
    /// name must match the filter (`.mcp.json`, not e.g. `file.txt`)
    /// or the filter drops it regardless of recursion mode, so this is
    /// the test that actually guards the constraint.
    #[test]
    #[ignore = "flaky on CI: OS file watcher may fail to initialize"]
    fn nested_subdir_change_does_not_trigger() {
        let grok_home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let nested = cwd.path().join("some").join("deep").join("nested");
        fs::create_dir_all(&nested).unwrap();

        let (_w, mut rx) = ConfigFileWatcher::start(
            grok_home.path(),
            &[],
            Some(cwd.path()),
            Some(Duration::from_millis(100)),
        )
        .expect("watcher should start");

        // Write a file whose name DOES match the watcher filter —
        // under recursive mode this would surface
        // as a `ConfigChangeEvent`; under non-recursive mode no
        // event must reach `rx`.
        fs::write(
            nested.join(".mcp.json"),
            r#"{"mcpServers": {"x": {"command": "/bin/true"}}}"#,
        )
        .unwrap();
        wait_ms(500);

        assert!(
            rx.try_recv().is_err(),
            "non-recursive watch must not surface .mcp.json events from <cwd>/some/deep/nested/"
        );
    }

    /// [`ConfigFileWatcher::watch_path`] registered after
    /// `start` must light up `<new_cwd>/.grok/config.toml` writes
    /// identically to a cwd passed in at `start`. Exercises the
    /// session-open registration path where the leader learns about a
    /// new project root after the watcher is already running.
    #[test]
    #[cfg_attr(
        target_os = "macos",
        ignore = "flaky on macOS: FSEvents does not reliably deliver events in test harness"
    )]
    fn watch_path_dynamic_registration() {
        let grok_home = TempDir::new().unwrap();
        let new_cwd = TempDir::new().unwrap();
        let project_grok = new_cwd.path().join(".grok");
        fs::create_dir_all(&project_grok).unwrap();
        fs::write(project_grok.join("config.toml"), "").unwrap();

        let (mut watcher, mut rx) = ConfigFileWatcher::start(
            grok_home.path(),
            &[],
            None,
            Some(Duration::from_millis(100)),
        )
        .expect("watcher should start");

        watcher.watch_path(new_cwd.path());

        fs::write(
            project_grok.join("config.toml"),
            "[mcp_servers.y]\ncommand = \"/bin/true\"",
        )
        .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut found = false;
        while std::time::Instant::now() < deadline {
            if let Ok(evt) = rx.try_recv()
                && matches!(evt, ConfigChangeEvent::ProjectConfigChanged { .. })
            {
                found = true;
                break;
            }
            wait_ms(50);
        }
        assert!(
            found,
            "watch_path-registered cwd must surface ProjectConfigChanged within 2s"
        );
    }

    /// Bookkeeping-only (no OS event delivery, so deterministic on
    /// every platform): `watch_path` records the cwd in `watched_cwds`
    /// and is idempotent; `unwatch_path` removes it and is a no-op for
    /// an unknown cwd. Guards the set that backs `unwatch_path` and the
    /// `watch_path` de-dup.
    #[test]
    fn watch_and_unwatch_path_bookkeeping() {
        let grok_home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let Some((mut watcher, _rx)) = ConfigFileWatcher::start(
            grok_home.path(),
            &[],
            None,
            Some(Duration::from_millis(100)),
        ) else {
            // OS watcher unavailable in this environment; nothing to assert.
            return;
        };
        let p = cwd.path();
        assert!(!watcher.watched_cwds.contains(p));

        watcher.watch_path(p);
        assert!(watcher.watched_cwds.contains(p));

        // Idempotent: a second registration doesn't duplicate the entry.
        watcher.watch_path(p);
        assert_eq!(
            watcher
                .watched_cwds
                .iter()
                .filter(|c| c.as_path() == p)
                .count(),
            1,
        );

        // Unwatch removes it; a second unwatch is a no-op.
        watcher.unwatch_path(p);
        assert!(!watcher.watched_cwds.contains(p));
        watcher.unwatch_path(p);
        assert!(!watcher.watched_cwds.contains(p));
    }
}
