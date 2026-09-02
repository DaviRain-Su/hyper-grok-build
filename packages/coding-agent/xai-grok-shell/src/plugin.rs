//! Shared plugin install, uninstall, update, and marketplace operations (output-agnostic).
//!
//! Called by the CLI (`plugin_cmd.rs`).
//! The in-session slash commands (`acp_session.rs`) currently inline similar logic and should migrate here.
//!
//! Callers own output formatting and telemetry.

use std::path::{Path, PathBuf};

use xai_grok_agent::plugins::discovery::PluginScope;
use xai_grok_agent::plugins::git_install::{self, UpdateStatus};
use xai_grok_agent::plugins::install_registry::{
    InstallError, InstallKind, InstallRegistry, InstalledRepo, MarketplaceProvenance,
};
use xai_grok_plugin_marketplace::git::{self, SourceCacheLease};
use xai_grok_plugin_marketplace::{
    MarketplaceEntry, MarketplaceRelativePath, MarketplaceSource, SourceKind, install_resolve,
    installer, is_official_source_url, load_extra_sources_from_settings, load_sources,
    scan_marketplace,
};

// ── Helpers (internal) ──────────────────────────────────────────────

fn save_registry_or_warn(registry: &InstallRegistry) {
    if let Err(e) = registry.save() {
        tracing::warn!("failed to save install registry: {e}");
    }
}

// ── Install ─────────────────────────────────────────────────────────

pub struct InstallOutcome {
    pub repo_key: String,
    pub plugin_names: Vec<String>,
    pub warnings: Vec<String>,
    /// Whether the source was a local path (vs git). For telemetry `InstallKind`.
    pub is_local: bool,
}

/// Classify an install source as local (filesystem) vs git (remote) without installing.
/// Used for telemetry `install_kind` on the failure path, where no [`InstallOutcome`] is available.
pub(crate) fn install_source_is_local(source: &str, cwd: &Path) -> bool {
    matches!(
        git_install::parse_install_source(source, cwd),
        git_install::InstallSource::Local { .. }
    )
}

/// Parse, clone/symlink, register, and enable a plugin. Does not emit telemetry.
pub fn install_plugin(source: &str, cwd: &Path) -> Result<InstallOutcome, InstallError> {
    let install_source = git_install::parse_install_source(source, cwd);
    let is_local = matches!(install_source, git_install::InstallSource::Local { .. });
    let mut registry = InstallRegistry::load();

    let result =
        git_install::install_from_source(&install_source, &registry, marketplace_require_sha())?;

    let repo = git_install::build_installed_repo(&result, &install_source);
    registry.insert(result.repo_key.clone(), repo);
    save_registry_or_warn(&registry);

    let (plugin_names, post_warnings) = crate::config::post_install_plugin(&result.repo_key);

    Ok(InstallOutcome {
        repo_key: result.repo_key,
        plugin_names,
        warnings: post_warnings,
        is_local,
    })
}

// ── Uninstall ───────────────────────────────────────────────────────

pub struct UninstallOutcome {
    pub repo_key: String,
    pub removed_plugins: Vec<String>,
}

pub enum UninstallError {
    NotFound {
        name: String,
    },
    NeedsConfirm {
        name: String,
        repo_key: String,
        other_plugins: Vec<String>,
        total: usize,
    },
}

impl std::fmt::Display for UninstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { name } => {
                write!(
                    f,
                    "Plugin \"{name}\" not found.\n\
                     Run `grok plugin list` to see installed plugins."
                )
            }
            Self::NeedsConfirm {
                name,
                repo_key,
                other_plugins,
                total,
            } => {
                writeln!(
                    f,
                    "Plugin \"{name}\" belongs to repo \"{repo_key}\" which also contains:"
                )?;
                for p in other_plugins {
                    writeln!(f, "  - {p}")?;
                }
                writeln!(f)?;
                write!(f, "Uninstalling will remove all {total} plugin(s).")
            }
        }
    }
}

/// Find, remove, clean up, and deregister a plugin.
/// When `keep_data` is true, `~/.grok/plugin-data/<id>/` is preserved.
pub fn uninstall_plugin(
    name: &str,
    confirm: bool,
    keep_data: bool,
) -> Result<UninstallOutcome, UninstallError> {
    let mut registry = InstallRegistry::load();
    let (repo_key, repo) = match registry.find_plugin(name) {
        Some((k, r, _)) => (k.to_string(), r.clone()),
        None => {
            return Err(UninstallError::NotFound {
                name: name.to_string(),
            });
        }
    };

    let removed_plugins: Vec<String> = repo.plugins.keys().cloned().collect();

    if removed_plugins.len() > 1 && !confirm {
        let others: Vec<_> = removed_plugins
            .iter()
            .filter(|p| p.as_str() != name)
            .cloned()
            .collect();
        return Err(UninstallError::NeedsConfirm {
            name: name.to_string(),
            repo_key,
            other_plugins: others,
            total: removed_plugins.len(),
        });
    }

    if let Err(e) = git_install::remove_repo_path(&repo.path) {
        tracing::warn!("failed to remove repo path: {e}");
    }

    if !keep_data {
        // Plugins under $HOME are user-scope; everything else is config-path scope.
        let scope = match xai_dirs::home_dir() {
            Some(home) if repo.path.starts_with(&home) => PluginScope::User,
            _ => PluginScope::ConfigPath,
        };
        git_install::cleanup_plugin_data(&repo, scope);
    }

    registry.remove(&repo_key);
    save_registry_or_warn(&registry);

    Ok(UninstallOutcome {
        repo_key,
        removed_plugins,
    })
}

// ── Update ──────────────────────────────────────────────────────────

pub enum RepoUpdateOutcome {
    Updated {
        repo_key: String,
        old_commit: Option<String>,
        new_commit: Option<String>,
    },
    AlreadyUpToDate {
        repo_key: String,
    },
    Pinned {
        repo_key: String,
        ref_name: String,
    },
    LiveLocal {
        repo_key: String,
    },
    Failed {
        repo_key: String,
        error: String,
    },
}

pub enum UpdateError {
    NotFound { name: String },
}

pub enum PluginUpdateSelector {
    PluginName(String),
    RepoKey(String),
}

pub fn repo_update_requires_reload(outcome: &RepoUpdateOutcome) -> bool {
    matches!(outcome, RepoUpdateOutcome::Updated { .. })
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { name } => {
                write!(
                    f,
                    "Plugin \"{name}\" not found.\n\
                     Run `grok plugin list` to see installed plugins."
                )
            }
        }
    }
}

/// Apply an update result to the registry entry.
fn apply_update_to_registry(
    registry: &mut InstallRegistry,
    repo_key: &str,
    result: &git_install::UpdateResult,
) {
    let Some(entry) = registry.get_repo_mut(repo_key) else {
        return;
    };
    if let InstallKind::Git { ref mut commit, .. } = entry.kind {
        *commit = result.new_commit.clone().unwrap_or_default();
    }
    entry.updated_at = chrono::Utc::now().to_rfc3339();
    entry.plugins = git_install::repo_plugin_map(&result.plugins);
}

struct MarketplaceSourceRoot {
    path: PathBuf,
    _lease: Option<SourceCacheLease>,
}

fn update_marketplace_repo(
    registry: &mut InstallRegistry,
    repo: &InstalledRepo,
    source_cache: &mut std::collections::HashMap<String, MarketplaceSourceRoot>,
) -> Result<installer::MarketplaceUpdateResult, InstallError> {
    let provenance = repo
        .marketplace
        .clone()
        .ok_or_else(|| InstallError::InstallFailed {
            detail: "installed repo is missing marketplace provenance".into(),
        })?;
    let entry_path = MarketplaceRelativePath::parse(&provenance.plugin_subdir).map_err(|e| {
        InstallError::InstallFailed {
            detail: format!("invalid marketplace plugin path: {e}"),
        }
    })?;

    let cache_key = provenance.source_url_or_path.clone();
    if !source_cache.contains_key(&cache_key) {
        source_cache.insert(
            cache_key.clone(),
            marketplace_root_for_provenance(&provenance)?,
        );
    }
    let marketplace_root = source_cache
        .get(&cache_key)
        .unwrap_or_else(|| unreachable!());
    let scan = scan_marketplace(&marketplace_root.path);
    let entry = scan
        .entries
        .into_iter()
        .find(|entry| entry.relative_path == entry_path.as_str())
        .ok_or_else(|| InstallError::PluginNotFound {
            name: provenance.plugin_subdir.clone(),
        })?;

    let require_sha = crate::plugin::marketplace_require_sha();
    installer::update_from_marketplace_entry_transactional(
        &marketplace_root.path,
        &entry,
        provenance,
        registry,
        require_sha,
    )
}

fn marketplace_root_for_provenance(
    provenance: &xai_grok_agent::plugins::install_registry::MarketplaceProvenance,
) -> Result<MarketplaceSourceRoot, InstallError> {
    let source = &provenance.source_url_or_path;
    if let Some((url, branch)) = configured_marketplace_git_source(source) {
        let cache_root = git::default_cache_root();
        let lease = git::sync_source_cache_with_mode(
            &url,
            branch.as_deref(),
            &cache_root,
            git::SyncMode::Force,
        )
        .map_err(|e| InstallError::InstallFailed {
            detail: format!("Git sync failed: {e}"),
        })?;
        return Ok(MarketplaceSourceRoot {
            path: lease.path.clone(),
            _lease: Some(lease),
        });
    }

    if source.contains("://") || source.contains("git@") {
        let cache_root = git::default_cache_root();
        let lease =
            git::sync_source_cache_with_mode(source, None, &cache_root, git::SyncMode::Force)
                .map_err(|e| InstallError::InstallFailed {
                    detail: format!("Git sync failed: {e}"),
                })?;
        Ok(MarketplaceSourceRoot {
            path: lease.path.clone(),
            _lease: Some(lease),
        })
    } else {
        Ok(MarketplaceSourceRoot {
            path: PathBuf::from(source),
            _lease: None,
        })
    }
}

fn configured_marketplace_git_source(source_url_or_path: &str) -> Option<(String, Option<String>)> {
    load_marketplace_sources()
        .into_iter()
        .find_map(|source| match source.kind {
            SourceKind::Git { url, branch } if url == source_url_or_path => Some((url, branch)),
            _ => None,
        })
}

/// Update one or all installed plugins. Saves the registry once at the end.
pub fn update_plugins(name: Option<&str>) -> Result<Vec<RepoUpdateOutcome>, UpdateError> {
    update_plugins_by_selector(name.map(|name| PluginUpdateSelector::PluginName(name.to_string())))
}

pub(crate) fn update_plugins_by_selector(
    selector: Option<PluginUpdateSelector>,
) -> Result<Vec<RepoUpdateOutcome>, UpdateError> {
    let mut registry = InstallRegistry::load();
    let repos_to_update: Vec<(String, InstalledRepo)> = match selector {
        Some(PluginUpdateSelector::PluginName(plugin_name)) => {
            match registry.find_plugin(&plugin_name) {
                Some((key, repo, _)) => vec![(key.to_string(), repo.clone())],
                None => {
                    return Err(UpdateError::NotFound {
                        name: plugin_name.to_string(),
                    });
                }
            }
        }
        Some(PluginUpdateSelector::RepoKey(repo_key)) => match registry.get_repo(&repo_key) {
            Some(repo) => vec![(repo_key.to_string(), repo.clone())],
            None => {
                return Err(UpdateError::NotFound {
                    name: repo_key.to_string(),
                });
            }
        },
        None => registry
            .list()
            .into_iter()
            .map(|(k, r)| (k.to_string(), r.clone()))
            .collect(),
    };

    let mut outcomes = Vec::with_capacity(repos_to_update.len());
    let mut source_cache = std::collections::HashMap::new();

    for (repo_key, repo) in &repos_to_update {
        let outcome = if repo.marketplace.is_some() {
            match update_marketplace_repo(&mut registry, repo, &mut source_cache) {
                Ok(result) => {
                    if result.changed || result.reinstalled {
                        RepoUpdateOutcome::Updated {
                            repo_key: result.repo_key,
                            old_commit: result.old_version,
                            new_commit: result.new_version,
                        }
                    } else {
                        RepoUpdateOutcome::AlreadyUpToDate {
                            repo_key: result.repo_key,
                        }
                    }
                }
                Err(e) => RepoUpdateOutcome::Failed {
                    repo_key: repo_key.clone(),
                    error: e.to_string(),
                },
            }
        } else {
            match git_install::update_repo(repo_key, repo, marketplace_require_sha()) {
                Ok(UpdateStatus::Updated(result)) if result.changed => {
                    apply_update_to_registry(&mut registry, repo_key, &result);
                    RepoUpdateOutcome::Updated {
                        repo_key: repo_key.clone(),
                        old_commit: result.old_commit,
                        new_commit: result.new_commit,
                    }
                }
                Ok(UpdateStatus::Updated(_)) => RepoUpdateOutcome::AlreadyUpToDate {
                    repo_key: repo_key.clone(),
                },
                Ok(UpdateStatus::Pinned { ref_name }) => RepoUpdateOutcome::Pinned {
                    repo_key: repo_key.clone(),
                    ref_name,
                },
                Ok(UpdateStatus::LiveLocal) => RepoUpdateOutcome::LiveLocal {
                    repo_key: repo_key.clone(),
                },
                Err(e) => RepoUpdateOutcome::Failed {
                    repo_key: repo_key.clone(),
                    error: e.to_string(),
                },
            }
        };
        outcomes.push(outcome);
    }

    save_registry_or_warn(&registry);

    Ok(outcomes)
}

// ── Marketplace helpers ─────────────────────────────────────────────

/// Expand GitHub shorthand (user/repo) to `https://github.com/user/repo.git`.
pub fn normalize_git_url(input: &str) -> String {
    if !input.contains("://") && !input.contains("git@") {
        format!("https://github.com/{}.git", input.trim_end_matches(".git"))
    } else {
        input.to_string()
    }
}

/// Derive a display name from the last path segment of a URL.
pub fn name_from_url(url: &str) -> String {
    let name = url
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplit('/')
        .next()
        .unwrap_or("marketplace");
    if name.is_empty() {
        "marketplace".to_string()
    } else {
        name.to_string()
    }
}

/// Derive a display name from the last component of a local path.
pub fn name_from_path(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| "marketplace".to_string())
}

/// A `marketplace add` input, split into the two source kinds the config supports (`git = "..."` vs `path = "..."`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarketplaceAddInput {
    /// Local directory. Tilde-expanded and absolutized against the caller's cwd.
    LocalPath(PathBuf),
    /// Git URL or GitHub shorthand, normalized via [`normalize_git_url`].
    GitUrl(String),
}

/// Classify a `marketplace add` input as a local directory or a git URL.
///
/// The explicit path indicators are a leading `/`, `.`, `~`, `\`, or a Windows drive prefix.
/// They mirror `is_github_shorthand`'s path checks in `git_install::parse_install_source`.
/// Unlike `plugin install`, unmarked inputs (`foo`, `a/b/c`) keep the legacy git-URL normalization for back-compat.
/// Without this split, a path input would be mangled into `https://github.com/<path>.git` and only fail after network clone attempts.
pub fn classify_marketplace_add_input(input: &str, cwd: &Path) -> MarketplaceAddInput {
    if !looks_like_local_path(input) {
        return MarketplaceAddInput::GitUrl(normalize_git_url(input));
    }
    let path = if input.starts_with('~') {
        expand_tilde(input)
    } else {
        let p = PathBuf::from(input);
        if p.is_relative() { cwd.join(p) } else { p }
    };
    // Lexical cleanup only (`.` segments, trailing slashes; `..` is kept, no symlink resolution)
    // It keeps the stored string canonical enough for the writer's raw-string idempotency check to match the loader's PathBuf one
    MarketplaceAddInput::LocalPath(path.components().collect())
}

/// Expand a leading `~` to the home directory, the same expansion the marketplace loader applies to `path =` config entries.
fn expand_tilde(input: &str) -> PathBuf {
    match input.strip_prefix('~') {
        Some(rest) => xai_dirs::home_dir()
            .map(|h| h.join(rest.strip_prefix('/').unwrap_or(rest)))
            .unwrap_or_else(|| PathBuf::from(input)),
        None => PathBuf::from(input),
    }
}

/// Leading `/`, `.`, `~`, `\` (UNC), or a Windows drive prefix (`C:\`, `C:/`).
fn looks_like_local_path(s: &str) -> bool {
    if s.starts_with('/') || s.starts_with('.') || s.starts_with('~') || s.starts_with('\\') {
        return true;
    }
    let b = s.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\')
}

/// Classify an install error for telemetry. Strings match `acp_session.rs` exactly.
pub fn classify_install_error(err: &InstallError) -> String {
    match err {
        InstallError::AlreadyInstalled { .. } => "already_installed",
        InstallError::Io { .. } => "io",
        InstallError::Json { .. } => "json",
        InstallError::PluginNotFound { .. } => "not_found",
        InstallError::ShaMismatch { .. } => "sha_mismatch",
        InstallError::UnpinnedRemoteRefused { .. } => "unpinned_remote_refused",
        InstallError::InstallFailed { .. } => "install_failed",
    }
    .to_string()
}

// ── Marketplace plugin install (direct CLI install) ─────────────────

pub struct MarketplaceInstallOutcome {
    pub repo_key: String,
    pub plugin_names: Vec<String>,
    pub warnings: Vec<String>,
    pub source_display_name: String,
    pub plugin_subdir: String,
    pub source_is_git: bool,
    pub already_installed: bool,
    pub other_copies_note: Option<String>,
}

#[derive(Debug)]
pub enum MarketplaceInstallError {
    UnknownQualifier {
        qualifier: String,
        registered: Vec<String>,
    },
    AmbiguousQualifier {
        qualifier: String,
        sources: Vec<String>,
    },
    QualifiedNameNotFound {
        name: String,
        source_display: String,
    },
    NameNotFound {
        name: String,
        skipped_sources: Vec<String>,
    },
    NameAmbiguous {
        name: String,
        candidates: Vec<String>,
    },
    PartialScan {
        name: String,
        skipped_sources: Vec<String>,
    },
    Sync {
        source_display: String,
        detail: String,
    },
    Install(InstallError),
}

impl MarketplaceInstallError {
    /// Stable telemetry category, reusing [`classify_install_error`] for the underlying install failure.
    pub fn category(&self) -> String {
        match self {
            Self::UnknownQualifier { .. } => "unknown_marketplace".to_string(),
            Self::AmbiguousQualifier { .. } => "ambiguous_marketplace".to_string(),
            Self::QualifiedNameNotFound { .. } | Self::NameNotFound { .. } => {
                "not_found".to_string()
            }
            Self::NameAmbiguous { .. } => "ambiguous_plugin".to_string(),
            Self::PartialScan { .. } => "partial_scan".to_string(),
            Self::Sync { .. } => "sync_failed".to_string(),
            Self::Install(e) => classify_install_error(e),
        }
    }
}

impl std::fmt::Display for MarketplaceInstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownQualifier {
                qualifier,
                registered,
            } => {
                if registered.is_empty() {
                    write!(
                        f,
                        "Unknown marketplace \"{qualifier}\". No marketplaces are registered; \
                         add one with `grok plugin marketplace add`."
                    )
                } else {
                    let list = bullet_list(registered);
                    write!(
                        f,
                        "Unknown marketplace \"{qualifier}\".\n\
                         Registered marketplaces (pin with <name>@<qualifier>):\n{list}"
                    )
                }
            }
            Self::AmbiguousQualifier { qualifier, sources } => {
                let list = bullet_list(sources);
                write!(
                    f,
                    "Marketplace qualifier \"{qualifier}\" matches multiple registered sources \
                     that cannot be distinguished by qualifier:\n{list}\n\
                     Rename or remove one in your marketplace config so each source has a unique \
                     qualifier."
                )
            }
            Self::QualifiedNameNotFound {
                name,
                source_display,
            } => {
                write!(
                    f,
                    "No marketplace plugin named \"{name}\" in \"{source_display}\"."
                )
            }
            Self::NameNotFound {
                name,
                skipped_sources,
            } => {
                write!(
                    f,
                    "No marketplace plugin named \"{name}\" in any registered marketplace.\n\
                     Install a local directory with `grok plugin install ./{name}`, or add a \
                     source with `grok plugin marketplace add`."
                )?;
                if !skipped_sources.is_empty() {
                    write!(
                        f,
                        "\n({} marketplace source(s) could not be synced and were skipped: {})",
                        skipped_sources.len(),
                        skipped_sources.join(", "),
                    )?;
                }
                Ok(())
            }
            Self::NameAmbiguous { name, candidates } => {
                let list = bullet_list(candidates);
                write!(
                    f,
                    "Multiple marketplaces provide a plugin named \"{name}\":\n{list}\n\
                     Pin one with `grok plugin install {name}@<qualifier>`."
                )
            }
            Self::PartialScan {
                name,
                skipped_sources,
            } => {
                let list = bullet_list(skipped_sources);
                write!(
                    f,
                    "Couldn't scan every marketplace while resolving \"{name}\", so it can't be \
                     resolved safely. Unscanned source(s):\n{list}\n\
                     Retry, or pin the source explicitly with `grok plugin install {name}@<qualifier>`."
                )
            }
            Self::Sync {
                source_display,
                detail,
            } => {
                write!(
                    f,
                    "Failed to sync marketplace \"{source_display}\": {detail}"
                )
            }
            Self::Install(e) => write!(f, "{e}"),
        }
    }
}

fn bullet_list(items: &[String]) -> String {
    items
        .iter()
        .map(|item| format!("  - {item}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The require-sha pin policy for remote plugin code. Disk-only config and env, both tighten-only.
/// Neither a remote campaign nor the `GROK_CONFIG` / `GROK_CONFIG_PATH` overlay must be able to relax a local security policy.
/// An unreadable config falls back to the env setting.
/// Read from the overlay-free layer merge (the overlay-free contract in `ConfigLayers::env_overlay`).
/// That way an overlay `require_sha = false` cannot defeat a disk-set `true`.
pub(crate) fn marketplace_require_sha() -> bool {
    xai_grok_config::ConfigLayers::load()
        .map(|layers| {
            xai_grok_plugin_marketplace::load_require_sha(
                &layers.effective_config_base_without_overlay(),
            )
        })
        .unwrap_or_else(|_| xai_grok_plugin_marketplace::env_require_sha())
}

/// Marketplace sources from config.toml and settings JSON, unfiltered.
pub(crate) fn load_marketplace_sources() -> Vec<MarketplaceSource> {
    let config = crate::config::load_effective_config()
        .ok()
        .unwrap_or(toml::Value::Table(toml::map::Map::new()));
    let mut sources = load_sources(&config);
    sources.extend(load_extra_sources_from_settings(&sources));
    sources
}

/// Like [`load_marketplace_sources`] but drops git sources blocked by the managed `marketplace_allowlist`.
/// Install paths must use this so policy cannot be bypassed.
pub(crate) fn load_filtered_marketplace_sources() -> Vec<MarketplaceSource> {
    let allowlist =
        &xai_grok_workspace::permission::resolution::managed_settings().marketplace_allowlist;
    filter_sources_by_allowlist(load_marketplace_sources(), allowlist)
}

fn filter_sources_by_allowlist(
    mut sources: Vec<MarketplaceSource>,
    allowlist: &xai_grok_workspace::permission::resolution::MarketplaceAllowlist,
) -> Vec<MarketplaceSource> {
    if allowlist.is_restricted() {
        sources.retain(|source| match &source.kind {
            SourceKind::Git { url, .. } => {
                if allowlist.is_url_allowed(url) {
                    true
                } else {
                    tracing::warn!(
                        name = %source.name,
                        url,
                        reason = %allowlist.block_reason(),
                        "Marketplace source blocked by allowlist"
                    );
                    false
                }
            }
            SourceKind::Local { .. } => true,
        });
    }
    sources
}

fn registered_source_label(source: &MarketplaceSource) -> String {
    let qualifier = install_resolve::addressable_qualifier(source);
    format!("{} ({qualifier})", source.name)
}

fn candidate_label(source: &MarketplaceSource, name: &str) -> String {
    let qualifier = install_resolve::addressable_qualifier(source);
    format!("{} (pin: {name}@{qualifier})", source.name)
}

fn resolve_source_root_for_install(
    source: &MarketplaceSource,
    cache_root: &Path,
) -> Result<MarketplaceSourceRoot, String> {
    match &source.kind {
        SourceKind::Local { path } => {
            if path.is_dir() {
                Ok(MarketplaceSourceRoot {
                    path: path.clone(),
                    _lease: None,
                })
            } else {
                Err(format!(
                    "local source directory not found: {}",
                    path.display()
                ))
            }
        }
        SourceKind::Git { url, branch } => {
            let lease = git::sync_source_cache_with_mode(
                url,
                branch.as_deref(),
                cache_root,
                git::SyncMode::UseTtl,
            )?;
            Ok(MarketplaceSourceRoot {
                path: lease.path.clone(),
                _lease: Some(lease),
            })
        }
    }
}

#[derive(Debug)]
struct InstallPlan {
    source_index: usize,
    entry: MarketplaceEntry,
    other_copies_note: Option<String>,
    /// Sources skipped during a bare-name scan because they failed to sync.
    skipped_sources: Vec<String>,
}

/// Map a marketplace ref to the source and entry to install, or a typed error.
/// Pure over `sources` and the `scan` closure so it is unit-testable.
fn plan_install(
    sources: &[MarketplaceSource],
    name: &str,
    qualifier: Option<&str>,
    mut scan: impl FnMut(&MarketplaceSource) -> Result<Vec<MarketplaceEntry>, String>,
) -> Result<InstallPlan, MarketplaceInstallError> {
    match qualifier {
        Some(qualifier) => {
            let index = install_resolve::resolve_qualified_source(qualifier, sources)
                .map_err(|e| map_qualifier_resolve_error(qualifier, sources, e))?;
            let source = &sources[index];
            let entry = scan(source)
                .map_err(|detail| MarketplaceInstallError::Sync {
                    source_display: source.name.clone(),
                    detail,
                })?
                .into_iter()
                .find(|entry| entry.name.eq_ignore_ascii_case(name))
                .ok_or_else(|| MarketplaceInstallError::QualifiedNameNotFound {
                    name: name.to_string(),
                    source_display: source.name.clone(),
                })?;
            Ok(InstallPlan {
                source_index: index,
                entry,
                other_copies_note: None,
                skipped_sources: Vec::new(),
            })
        }
        None => {
            let mut owned: Vec<(usize, MarketplaceEntry)> = Vec::new();
            let mut skipped_sources = Vec::new();
            for (index, source) in sources.iter().enumerate() {
                match scan(source) {
                    Ok(entries) => {
                        for entry in entries {
                            owned.push((index, entry));
                        }
                    }
                    Err(_) => skipped_sources.push(source.name.clone()),
                }
            }
            let scanned: Vec<install_resolve::ScannedEntry> = owned
                .iter()
                .map(|(index, entry)| install_resolve::ScannedEntry {
                    source: &sources[*index],
                    entry,
                })
                .collect();
            let selection = match install_resolve::select_bare_name(name, &scanned) {
                Ok(selection) => selection,
                Err(install_resolve::BareNameError::NotFound) => {
                    drop(scanned);
                    return Err(if skipped_sources.is_empty() {
                        MarketplaceInstallError::NameNotFound {
                            name: name.to_string(),
                            skipped_sources,
                        }
                    } else {
                        MarketplaceInstallError::PartialScan {
                            name: name.to_string(),
                            skipped_sources,
                        }
                    });
                }
                Err(install_resolve::BareNameError::Ambiguous { matched }) => {
                    if !skipped_sources.is_empty() {
                        drop(scanned);
                        return Err(MarketplaceInstallError::PartialScan {
                            name: name.to_string(),
                            skipped_sources,
                        });
                    }
                    let candidates = matched
                        .iter()
                        .map(|&i| candidate_label(scanned[i].source, name))
                        .collect();
                    drop(scanned);
                    return Err(MarketplaceInstallError::NameAmbiguous {
                        name: name.to_string(),
                        candidates,
                    });
                }
            };
            let chosen_source_index = owned[selection.chosen].0;
            let chosen_is_official = match &sources[chosen_source_index].kind {
                SourceKind::Git { url, .. } => is_official_source_url(url),
                SourceKind::Local { .. } => false,
            };
            let other_copies_note = (selection.other_count > 0).then(|| {
                format!(
                    "Note: \"{name}\" is also available from {} other marketplace(s); \
                     pin a specific one with `{name}@<qualifier>`.",
                    selection.other_count
                )
            });
            drop(scanned);
            if !chosen_is_official && !skipped_sources.is_empty() {
                return Err(MarketplaceInstallError::PartialScan {
                    name: name.to_string(),
                    skipped_sources,
                });
            }
            let (source_index, entry) = owned.swap_remove(selection.chosen);
            Ok(InstallPlan {
                source_index,
                entry,
                other_copies_note,
                skipped_sources,
            })
        }
    }
}

/// Install a plugin by marketplace name, optionally pinned via `qualifier` (`owner/repo` or `local/<slug>`).
/// Loads allowlist-filtered sources and delegates selection to [`plan_install`].
pub fn install_marketplace_plugin(
    name: &str,
    qualifier: Option<&str>,
) -> Result<MarketplaceInstallOutcome, MarketplaceInstallError> {
    let sources = load_filtered_marketplace_sources();
    let mut registry = InstallRegistry::load();
    let cache_root = git::default_cache_root();
    install_marketplace_plugin_with(
        &sources,
        &mut registry,
        &cache_root,
        name,
        qualifier,
        crate::config::post_install_plugin,
    )
}

fn install_marketplace_plugin_with(
    sources: &[MarketplaceSource],
    registry: &mut InstallRegistry,
    cache_root: &Path,
    name: &str,
    qualifier: Option<&str>,
    post_install: impl Fn(&str) -> (Vec<String>, Vec<String>),
) -> Result<MarketplaceInstallOutcome, MarketplaceInstallError> {
    let plan = plan_install(sources, name, qualifier, |source| {
        resolve_source_root_for_install(source, cache_root)
            .map(|root| scan_marketplace(&root.path).entries)
    })?;

    let source = &sources[plan.source_index];
    let root = resolve_source_root_for_install(source, cache_root).map_err(|detail| {
        MarketplaceInstallError::Sync {
            source_display: source.name.clone(),
            detail,
        }
    })?;
    let mut outcome =
        install_marketplace_entry(source, &root.path, &plan.entry, registry, post_install)?;
    if !outcome.already_installed {
        outcome.other_copies_note = plan.other_copies_note;
        for skipped in plan.skipped_sources {
            outcome.warnings.push(format!(
                "marketplace source \"{skipped}\" could not be synced and was skipped"
            ));
        }
    }
    Ok(outcome)
}

pub fn resolve_marketplace_source_name(
    name: &str,
    qualifier: Option<&str>,
) -> Result<String, MarketplaceInstallError> {
    let sources = load_filtered_marketplace_sources();
    let cache_root = git::default_cache_root();
    resolve_marketplace_source_name_with(&sources, &cache_root, name, qualifier)
}

fn resolve_marketplace_source_name_with(
    sources: &[MarketplaceSource],
    cache_root: &Path,
    name: &str,
    qualifier: Option<&str>,
) -> Result<String, MarketplaceInstallError> {
    let plan = plan_install(sources, name, qualifier, |source| {
        resolve_source_root_for_install(source, cache_root)
            .map(|root| scan_marketplace(&root.path).entries)
    })?;
    Ok(sources[plan.source_index].name.clone())
}

pub fn resolve_qualified_source_name(qualifier: &str) -> Result<String, MarketplaceInstallError> {
    resolve_qualified_source_name_with(&load_filtered_marketplace_sources(), qualifier)
}

fn resolve_qualified_source_name_with(
    sources: &[MarketplaceSource],
    qualifier: &str,
) -> Result<String, MarketplaceInstallError> {
    let index = install_resolve::resolve_qualified_source(qualifier, sources)
        .map_err(|e| map_qualifier_resolve_error(qualifier, sources, e))?;
    Ok(sources[index].name.clone())
}

fn map_qualifier_resolve_error(
    qualifier: &str,
    sources: &[MarketplaceSource],
    e: install_resolve::QualifierResolveError,
) -> MarketplaceInstallError {
    use install_resolve::QualifierResolveError;
    match e {
        QualifierResolveError::Unknown => MarketplaceInstallError::UnknownQualifier {
            qualifier: qualifier.to_string(),
            registered: sources.iter().map(registered_source_label).collect(),
        },
        QualifierResolveError::Ambiguous(indices) => MarketplaceInstallError::AmbiguousQualifier {
            qualifier: qualifier.to_string(),
            sources: indices.iter().map(|&i| sources[i].name.clone()).collect(),
        },
    }
}

fn install_marketplace_entry(
    source: &MarketplaceSource,
    marketplace_root: &Path,
    entry: &MarketplaceEntry,
    registry: &mut InstallRegistry,
    post_install: impl Fn(&str) -> (Vec<String>, Vec<String>),
) -> Result<MarketplaceInstallOutcome, MarketplaceInstallError> {
    let source_is_git = matches!(&source.kind, SourceKind::Git { .. });
    let source_identity = match &source.kind {
        SourceKind::Git { url, .. } => url.clone(),
        SourceKind::Local { path } => path.display().to_string(),
    };
    let plugin_subdir = MarketplaceRelativePath::parse(&entry.relative_path)
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|_| entry.relative_path.clone());

    if let Some((repo_key, _version)) =
        installer::find_installed_marketplace_plugin(registry, &source_identity, &plugin_subdir)
    {
        let plugin_names = registry
            .get_repo(&repo_key)
            .map(|repo| repo.plugins.keys().cloned().collect())
            .unwrap_or_default();
        return Ok(MarketplaceInstallOutcome {
            repo_key,
            plugin_names,
            warnings: Vec::new(),
            source_display_name: source.name.clone(),
            plugin_subdir,
            source_is_git,
            already_installed: true,
            other_copies_note: None,
        });
    }

    let provenance = MarketplaceProvenance {
        source_url_or_path: source_identity,
        source_display_name: source.name.clone(),
        plugin_subdir: plugin_subdir.clone(),
    };

    let result = if let Some(remote_url) = entry.remote_url.as_deref() {
        let require_sha = crate::plugin::marketplace_require_sha();
        installer::install_from_remote_url(
            remote_url,
            entry.remote_ref.as_deref(),
            entry.remote_sha.as_deref(),
            entry.remote_subdir.as_deref(),
            &plugin_subdir,
            provenance,
            registry,
            require_sha,
        )
    } else {
        installer::install_from_marketplace(marketplace_root, &plugin_subdir, provenance, registry)
    };

    let repo_key = match result {
        Ok(installer::MarketplaceInstallResult::Installed { repo_key })
        | Ok(installer::MarketplaceInstallResult::AlreadyInstalled { repo_key }) => repo_key,
        Err(e) => return Err(MarketplaceInstallError::Install(e)),
    };

    let (plugin_names, warnings) = post_install(&repo_key);

    Ok(MarketplaceInstallOutcome {
        repo_key,
        plugin_names,
        warnings,
        source_display_name: source.name.clone(),
        plugin_subdir,
        source_is_git,
        already_installed: false,
        other_copies_note: None,
    })
}

/// Remove all plugins installed from a marketplace source. Returns removed repo keys.
pub fn uninstall_marketplace_source_plugins(source_identity: &str) -> Vec<String> {
    let mut registry = InstallRegistry::load();
    let to_remove: Vec<(String, std::path::PathBuf, InstalledRepo)> = registry
        .list()
        .iter()
        .filter_map(|(key, repo)| {
            repo.marketplace.as_ref().and_then(|mp| {
                if mp.source_url_or_path == source_identity {
                    Some((key.to_string(), repo.path.clone(), (*repo).clone()))
                } else {
                    None
                }
            })
        })
        .collect();

    for (key, path, repo) in &to_remove {
        if let Err(e) = git_install::remove_repo_path(path) {
            tracing::warn!("failed to remove plugin dir for {key}: {e}");
        }
        let scope = match xai_dirs::home_dir() {
            Some(home) if path.starts_with(&home) => PluginScope::User,
            _ => PluginScope::ConfigPath,
        };
        git_install::cleanup_plugin_data(repo, scope);
        registry.remove(key);
    }

    if !to_remove.is_empty() {
        save_registry_or_warn(&registry);
    }

    to_remove.into_iter().map(|(key, _, _)| key).collect()
}

/// Remove a `[[marketplace.sources]]` entry matching `git` or `path`.
/// Returns `Some(new_content)` on removal, `None` if not found or unparseable.
pub fn remove_toml_marketplace_block(content: &str, source_identity: &str) -> Option<String> {
    let mut doc: toml_edit::DocumentMut = content.parse().ok()?;

    let sources = doc
        .get_mut("marketplace")?
        .get_mut("sources")?
        .as_array_of_tables_mut()?;

    let identity_normalized = source_identity.trim_end_matches(".git");
    let idx = sources.iter().position(|entry| {
        if let Some(git) = entry.get("git").and_then(|v| v.as_str()) {
            return git.trim_end_matches(".git") == identity_normalized;
        }
        if let Some(path) = entry.get("path").and_then(|v| v.as_str()) {
            // The identity comes from a loaded source, whose `~` was expanded; match hand-written `path = "~/x"` entries by expanding them too
            return path == source_identity || expand_tilde(path) == Path::new(source_identity);
        }
        false
    })?;

    sources.remove(idx);

    // Keep other `[marketplace]` keys (the sticky official_marketplace_auto_installed flag) when `sources` empties
    // Drop the table only when fully empty
    // Otherwise removing an unrelated source wipes the flag and auto-register re-adds it
    let sources_now_empty = doc
        .get("marketplace")
        .and_then(|m| m.get("sources"))
        .and_then(|s| s.as_array_of_tables())
        .is_some_and(|a| a.is_empty());
    if sources_now_empty
        && let Some(marketplace) = doc.get_mut("marketplace").and_then(|m| m.as_table_mut())
    {
        marketplace.remove("sources");
        if marketplace.is_empty() {
            doc.remove("marketplace");
        }
    }

    Some(doc.to_string())
}

/// Try removing a source from `settings.json` / `known_marketplaces.json` under
/// `~/.grok/` and `~/.claude/`. Returns `true` if removed from at least one file.
pub fn try_remove_source_from_json_files(source_url_or_path: &str) -> bool {
    // Resolve user grok via user_grok_home() (None when no home resolves) and home separately
    // Removal then still runs from $GROK_HOME when no home dir exists, and never touches a cwd-relative .grok
    let home = xai_dirs::home_dir();
    let grok = xai_grok_config::user_grok_home();

    let mut settings_candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(ref grok) = grok {
        settings_candidates.push(grok.join("settings.local.json"));
        settings_candidates.push(grok.join("settings.json"));
    }
    if let Some(ref home) = home {
        settings_candidates.push(home.join(".claude").join("settings.local.json"));
        settings_candidates.push(home.join(".claude").join("settings.json"));
    }

    let mut known_candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(ref grok) = grok {
        known_candidates.push(grok.join("plugins").join("known_marketplaces.json"));
    }
    if let Some(ref home) = home {
        known_candidates.push(
            home.join(".claude")
                .join("plugins")
                .join("known_marketplaces.json"),
        );
    }

    let mut removed = false;

    for path in &settings_candidates {
        if try_remove_from_json_object(path, Some("extraKnownMarketplaces"), source_url_or_path) {
            removed = true;
        }
    }

    for path in &known_candidates {
        if try_remove_from_json_object(path, None, source_url_or_path) {
            removed = true;
        }
    }

    removed
}

/// Check whether a JSON source config matches a URL/path identity.
fn json_source_matches(config: &serde_json::Value, identity: &str) -> bool {
    let source_obj = match config.get("source") {
        Some(v) if v.is_string() => config,
        Some(v) if v.is_object() => v,
        _ => return false,
    };
    let Some(source_type) = source_obj.get("source").and_then(|v| v.as_str()) else {
        return false;
    };
    match source_type {
        "git" => source_obj
            .get("url")
            .and_then(|v| v.as_str())
            .is_some_and(|u| u.trim_end_matches(".git") == identity.trim_end_matches(".git")),
        "github" => source_obj
            .get("repo")
            .and_then(|v| v.as_str())
            .is_some_and(|repo| {
                let expanded = format!("https://github.com/{repo}.git");
                expanded.trim_end_matches(".git") == identity.trim_end_matches(".git")
            }),
        "local" => source_obj
            .get("path")
            .and_then(|v| v.as_str())
            .is_some_and(|p| p == identity),
        _ => false,
    }
}

/// Remove a matching source entry from a JSON file. Returns `true` if removed.
fn try_remove_from_json_object(
    path: &Path,
    nested_key: Option<&str>,
    source_url_or_path: &str,
) -> bool {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let mut json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return false,
    };

    let map = if let Some(key) = nested_key {
        match json.get_mut(key).and_then(|v| v.as_object_mut()) {
            Some(m) => m,
            None => return false,
        }
    } else {
        match json.as_object_mut() {
            Some(m) => m,
            None => return false,
        }
    };

    let matching_key = map.iter().find_map(|(name, config)| {
        if json_source_matches(config, source_url_or_path) {
            Some(name.clone())
        } else {
            None
        }
    });

    let Some(key) = matching_key else {
        return false;
    };

    map.remove(&key);

    match serde_json::to_string_pretty(&json) {
        Ok(new_content) => {
            if std::fs::write(path, format!("{new_content}\n")).is_ok() {
                tracing::info!(key = %key, "removed marketplace source from JSON file");
                true
            } else {
                false
            }
        }
        Err(_) => false,
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn normalize_github_shorthand() {
        assert_eq!(
            normalize_git_url("user/repo"),
            "https://github.com/user/repo.git"
        );
        // The .git suffix is not doubled
        assert_eq!(
            normalize_git_url("user/repo.git"),
            "https://github.com/user/repo.git"
        );
    }

    #[test]
    fn classify_add_input_git_urls_and_shorthand() {
        let cwd = Path::new("/work");
        for input in [
            "user/repo",
            "https://github.com/user/repo.git",
            "git@github.com:user/repo.git",
            "https://example.com/plugins.git",
        ] {
            assert!(
                matches!(
                    classify_marketplace_add_input(input, cwd),
                    MarketplaceAddInput::GitUrl(_)
                ),
                "expected git classification for {input}"
            );
        }
        // Shorthand still normalizes.
        assert_eq!(
            classify_marketplace_add_input("user/repo", cwd),
            MarketplaceAddInput::GitUrl("https://github.com/user/repo.git".into())
        );
    }

    #[test]
    fn classify_add_input_local_paths() {
        let cwd = Path::new("/work");
        assert_eq!(
            classify_marketplace_add_input("/abs/plugins", cwd),
            MarketplaceAddInput::LocalPath(PathBuf::from("/abs/plugins"))
        );
        // Relative paths absolutize against cwd (leading `./` trimmed).
        assert_eq!(
            classify_marketplace_add_input("./plugins", cwd),
            MarketplaceAddInput::LocalPath(PathBuf::from("/work/plugins"))
        );
        assert_eq!(
            classify_marketplace_add_input("../plugins", cwd),
            MarketplaceAddInput::LocalPath(PathBuf::from("/work/../plugins"))
        );
        // Tilde expands to home.
        if let Some(home) = xai_dirs::home_dir() {
            assert_eq!(
                classify_marketplace_add_input("~/plugins", cwd),
                MarketplaceAddInput::LocalPath(home.join("plugins"))
            );
        }
        // Windows drive prefix is a path, not a github shorthand.
        assert!(matches!(
            classify_marketplace_add_input("C:\\plugins", cwd),
            MarketplaceAddInput::LocalPath(_)
        ));
    }

    #[test]
    fn name_from_path_uses_last_component() {
        assert_eq!(name_from_path(Path::new("/a/b/my-plugins")), "my-plugins");
        assert_eq!(name_from_path(Path::new("/a/b/my-plugins/")), "my-plugins");
        assert_eq!(name_from_path(Path::new("/")), "marketplace");
    }

    #[test]
    fn name_from_url_extracts_last_segment() {
        assert_eq!(
            name_from_url("https://github.com/org/my-marketplace.git"),
            "my-marketplace"
        );
    }

    #[test]
    fn name_from_url_edge_cases() {
        assert_eq!(name_from_url("https://github.com/org/repo/"), "repo"); // trailing slash is trimmed
        assert_eq!(name_from_url(""), "marketplace"); // empty fallback
    }

    #[test]
    fn classify_error_strings_match_canonical() {
        // Must match acp_session.rs::classify_install_error exactly; that prevents telemetry drift
        assert_eq!(
            classify_install_error(&InstallError::AlreadyInstalled { key: "k".into() }),
            "already_installed"
        );
        assert_eq!(
            classify_install_error(&InstallError::Io {
                path: "p".into(),
                source: std::io::Error::other("x")
            }),
            "io"
        );
        assert_eq!(
            classify_install_error(&InstallError::Json { detail: "x".into() }),
            "json"
        );
        assert_eq!(
            classify_install_error(&InstallError::PluginNotFound { name: "x".into() }),
            "not_found"
        );
        assert_eq!(
            classify_install_error(&InstallError::ShaMismatch {
                expected: "a".into(),
                actual: "b".into()
            }),
            "sha_mismatch"
        );
        assert_eq!(
            classify_install_error(&InstallError::UnpinnedRemoteRefused {
                plugin: "p".into(),
                url: "u".into()
            }),
            "unpinned_remote_refused"
        );
        assert_eq!(
            classify_install_error(&InstallError::InstallFailed { detail: "x".into() }),
            "install_failed"
        );
    }

    #[test]
    fn update_requires_reload_for_changed_repo_updates_only() {
        assert!(repo_update_requires_reload(&RepoUpdateOutcome::Updated {
            repo_key: "git".into(),
            old_commit: Some("a".into()),
            new_commit: Some("b".into()),
        }));
        assert!(!repo_update_requires_reload(
            &RepoUpdateOutcome::LiveLocal {
                repo_key: "local".into(),
            }
        ));
    }

    #[test]
    fn non_marketplace_local_update_remains_noop() {
        let repo = InstalledRepo {
            kind: InstallKind::Local {
                source_path: PathBuf::from("/tmp/plugin"),
                subdir: None,
            },
            installed_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            path: PathBuf::from("/tmp/installed"),
            plugins: HashMap::new(),
            marketplace: None,
        };
        let status = git_install::update_repo("local", &repo, false).unwrap();
        assert!(matches!(status, UpdateStatus::LiveLocal));
    }

    #[test]
    fn remove_toml_selects_correct_entry() {
        let content = "[[marketplace.sources]]\nname = \"a\"\ngit = \"https://a.com\"\n\n\
                       [[marketplace.sources]]\nname = \"b\"\ngit = \"https://b.com\"\n";
        let new = remove_toml_marketplace_block(content, "https://a.com").unwrap();
        assert!(!new.contains("\"a\"") && new.contains("\"b\""), "{new}");
    }

    #[test]
    fn remove_toml_no_match_returns_none() {
        let content = "[[marketplace.sources]]\nname = \"x\"\ngit = \"https://a.com\"\n";
        assert!(remove_toml_marketplace_block(content, "https://nope.com").is_none());
    }

    #[test]
    fn remove_toml_matches_tilde_path_entry_by_expanded_identity() {
        // Loaded sources carry expanded paths, so removal by identity must still find a hand-written `path = "~/x"` entry
        let Some(home) = xai_dirs::home_dir() else {
            return;
        };
        let content = "[[marketplace.sources]]\nname = \"dev\"\npath = \"~/dev/plugins\"\n";
        let identity = home.join("dev/plugins").display().to_string();
        let new = remove_toml_marketplace_block(content, &identity).unwrap();
        assert!(!new.contains("dev/plugins"), "{new}");
    }

    #[test]
    fn remove_toml_cleans_empty_section() {
        let content = "[[marketplace.sources]]\nname = \"x\"\ngit = \"https://a.com\"\n";
        let new = remove_toml_marketplace_block(content, "https://a.com").unwrap();
        assert!(!new.contains("marketplace"), "{new}");
    }

    #[test]
    fn remove_toml_preserves_sibling_keys_when_sources_empty() {
        let content = "[marketplace]\nofficial_marketplace_auto_installed = true\n\n\
                       [[marketplace.sources]]\nname = \"x\"\ngit = \"https://a.com\"\n";
        let new = remove_toml_marketplace_block(content, "https://a.com").unwrap();
        assert!(
            new.contains("official_marketplace_auto_installed"),
            "sticky flag must survive removing the last source: {new}"
        );
        assert!(
            !new.contains("[[marketplace.sources]]"),
            "empty sources array should be dropped: {new}"
        );
    }

    #[test]
    fn json_source_matches_with_git_normalization() {
        let config = serde_json::json!({
            "source": { "source": "git", "url": "https://github.com/org/repo.git" }
        });
        assert!(json_source_matches(
            &config,
            "https://github.com/org/repo.git"
        ));
        assert!(json_source_matches(&config, "https://github.com/org/repo")); // .git normalization
        assert!(!json_source_matches(&config, "https://other.com"));
    }

    #[test]
    fn json_source_matches_github_shorthand() {
        let config = serde_json::json!({
            "source": { "source": "github", "repo": "org/repo" }
        });
        assert!(json_source_matches(
            &config,
            "https://github.com/org/repo.git"
        ));
    }

    fn git_source(name: &str, url: &str) -> MarketplaceSource {
        MarketplaceSource {
            name: name.into(),
            kind: SourceKind::Git {
                url: url.into(),
                branch: None,
            },
        }
    }

    fn local_source(name: &str, path: &str) -> MarketplaceSource {
        MarketplaceSource {
            name: name.into(),
            kind: SourceKind::Local {
                path: PathBuf::from(path),
            },
        }
    }

    #[test]
    fn registered_source_label_uses_addressable_qualifier() {
        assert_eq!(
            registered_source_label(&git_source(
                "xAI Official",
                "https://github.com/xai-org/plugin-marketplace.git"
            )),
            "xAI Official (xai-org/plugin-marketplace)"
        );
        assert_eq!(
            registered_source_label(&local_source("Local Dev", "/tmp/p")),
            "Local Dev (local/local-dev)"
        );
    }

    #[test]
    fn registered_source_label_uses_git_slug_for_non_github_git() {
        assert_eq!(
            registered_source_label(&git_source("Internal", "https://git.example.com/x/y.git")),
            "Internal (git/internal)"
        );
    }

    #[test]
    fn candidate_label_includes_pin_hint() {
        assert_eq!(
            candidate_label(
                &git_source(
                    "xAI Official",
                    "https://github.com/xai-org/plugin-marketplace.git"
                ),
                "sentry"
            ),
            "xAI Official (pin: sentry@xai-org/plugin-marketplace)"
        );
        assert_eq!(
            candidate_label(&local_source("Local Dev", "/tmp/p"), "sentry"),
            "Local Dev (pin: sentry@local/local-dev)"
        );
    }

    #[test]
    fn unknown_qualifier_error_lists_registered_marketplaces() {
        let err = MarketplaceInstallError::UnknownQualifier {
            qualifier: "acme/repo".into(),
            registered: vec![
                "xAI Official (xai-org/plugin-marketplace)".into(),
                "Local Dev (local/local-dev)".into(),
            ],
        };
        let msg = err.to_string();
        assert!(msg.contains("Unknown marketplace \"acme/repo\""), "{msg}");
        assert!(
            msg.contains("  - xAI Official (xai-org/plugin-marketplace)"),
            "{msg}"
        );
        assert!(msg.contains("  - Local Dev (local/local-dev)"), "{msg}");
    }

    #[test]
    fn ambiguous_qualifier_error_lists_source_names() {
        let err = MarketplaceInstallError::AmbiguousQualifier {
            qualifier: "xai-org/plugin-marketplace".into(),
            sources: vec!["Mirror A".into(), "Mirror B".into()],
        };
        let msg = err.to_string();
        assert!(
            msg.contains("cannot be distinguished by qualifier"),
            "{msg}"
        );
        assert!(msg.contains("Rename or remove one"), "{msg}");
        assert!(msg.contains("  - Mirror A"), "{msg}");
        assert!(msg.contains("  - Mirror B"), "{msg}");
    }

    #[test]
    fn name_not_found_error_hints_local_dir_and_add_source() {
        let err = MarketplaceInstallError::NameNotFound {
            name: "sentry".into(),
            skipped_sources: vec![],
        };
        let msg = err.to_string();
        assert!(msg.contains("grok plugin install ./sentry"), "{msg}");
        assert!(msg.contains("grok plugin marketplace add"), "{msg}");
        assert!(!msg.contains("could not be synced"), "{msg}");
    }

    #[test]
    fn name_not_found_error_reports_skipped_sources() {
        let err = MarketplaceInstallError::NameNotFound {
            name: "sentry".into(),
            skipped_sources: vec!["Flaky Remote".into()],
        };
        let msg = err.to_string();
        assert!(
            msg.contains("could not be synced and were skipped: Flaky Remote"),
            "{msg}"
        );
    }

    #[test]
    fn name_ambiguous_error_lists_candidates_and_pin_hint() {
        let err = MarketplaceInstallError::NameAmbiguous {
            name: "sentry".into(),
            candidates: vec!["xAI Official (pin: sentry@xai-org/plugin-marketplace)".into()],
        };
        let msg = err.to_string();
        assert!(
            msg.contains("Multiple marketplaces provide a plugin named \"sentry\""),
            "{msg}"
        );
        assert!(
            msg.contains("  - xAI Official (pin: sentry@xai-org/plugin-marketplace)"),
            "{msg}"
        );
        assert!(
            msg.contains("grok plugin install sentry@<qualifier>"),
            "{msg}"
        );
    }

    #[test]
    fn marketplace_install_error_category_matches_variant() {
        assert_eq!(
            MarketplaceInstallError::UnknownQualifier {
                qualifier: "x".into(),
                registered: vec![],
            }
            .category(),
            "unknown_marketplace"
        );
        assert_eq!(
            MarketplaceInstallError::AmbiguousQualifier {
                qualifier: "x".into(),
                sources: vec![],
            }
            .category(),
            "ambiguous_marketplace"
        );
        assert_eq!(
            MarketplaceInstallError::QualifiedNameNotFound {
                name: "x".into(),
                source_display: "s".into(),
            }
            .category(),
            "not_found"
        );
        assert_eq!(
            MarketplaceInstallError::NameNotFound {
                name: "x".into(),
                skipped_sources: vec![],
            }
            .category(),
            "not_found"
        );
        assert_eq!(
            MarketplaceInstallError::NameAmbiguous {
                name: "x".into(),
                candidates: vec![],
            }
            .category(),
            "ambiguous_plugin"
        );
        assert_eq!(
            MarketplaceInstallError::PartialScan {
                name: "x".into(),
                skipped_sources: vec![],
            }
            .category(),
            "partial_scan"
        );
        assert_eq!(
            MarketplaceInstallError::Sync {
                source_display: "s".into(),
                detail: "d".into(),
            }
            .category(),
            "sync_failed"
        );
        assert_eq!(
            MarketplaceInstallError::Install(InstallError::PluginNotFound { name: "x".into() })
                .category(),
            "not_found"
        );
    }

    fn mp_entry(name: &str) -> MarketplaceEntry {
        MarketplaceEntry {
            name: name.into(),
            version: None,
            description: None,
            category: None,
            author: None,
            tags: Vec::new(),
            keywords: Vec::new(),
            domains: Vec::new(),
            homepage: None,
            relative_path: format!("plugins/{name}"),
            skill_count: 0,
            has_hooks: false,
            has_agents: false,
            has_mcp: false,
            remote_url: None,
            remote_ref: None,
            remote_sha: None,
            remote_subdir: None,
            components: None,
        }
    }

    const OFFICIAL_URL: &str = "https://github.com/xai-org/plugin-marketplace.git";

    #[test]
    fn plan_install_qualifier_unknown_lists_registered_labels() {
        let sources = [
            git_source("xAI Official", OFFICIAL_URL),
            local_source("Local Dev", "/tmp/p"),
        ];
        let err = plan_install(&sources, "sentry", Some("acme/repo"), |_| Ok(Vec::new()))
            .expect_err("acme/repo is not a registered source");
        match err {
            MarketplaceInstallError::UnknownQualifier {
                qualifier,
                registered,
            } => {
                assert_eq!(qualifier, "acme/repo");
                assert_eq!(
                    registered,
                    vec![
                        "xAI Official (xai-org/plugin-marketplace)".to_string(),
                        "Local Dev (local/local-dev)".to_string(),
                    ]
                );
            }
            other => panic!("expected UnknownQualifier, got: {other}"),
        }
    }

    #[test]
    fn plan_install_qualifier_ambiguous_lists_source_names() {
        let sources = [
            git_source("Mirror A", OFFICIAL_URL),
            git_source("Mirror B", "git@github.com:xai-org/plugin-marketplace.git"),
        ];
        let err = plan_install(
            &sources,
            "sentry",
            Some("xai-org/plugin-marketplace"),
            |_| Ok(Vec::new()),
        )
        .expect_err("two sources share the owner/repo");
        match err {
            MarketplaceInstallError::AmbiguousQualifier { qualifier, sources } => {
                assert_eq!(qualifier, "xai-org/plugin-marketplace");
                assert_eq!(
                    sources,
                    vec!["Mirror A".to_string(), "Mirror B".to_string()]
                );
            }
            other => panic!("expected AmbiguousQualifier, got: {other}"),
        }
    }

    #[test]
    fn plan_install_qualifier_not_found_when_scan_lacks_name() {
        let sources = [git_source("xAI Official", OFFICIAL_URL)];
        let err = plan_install(
            &sources,
            "sentry",
            Some("xai-org/plugin-marketplace"),
            |_| Ok(vec![mp_entry("other")]),
        )
        .expect_err("source has no plugin named sentry");
        match err {
            MarketplaceInstallError::QualifiedNameNotFound {
                name,
                source_display,
            } => {
                assert_eq!(name, "sentry");
                assert_eq!(source_display, "xAI Official");
            }
            other => panic!("expected QualifiedNameNotFound, got: {other}"),
        }
    }

    #[test]
    fn plan_install_qualifier_sync_failure_is_hard_error() {
        let sources = [git_source("xAI Official", OFFICIAL_URL)];
        let err = plan_install(
            &sources,
            "sentry",
            Some("xai-org/plugin-marketplace"),
            |_| Err("network down".to_string()),
        )
        .expect_err("sync failed");
        match err {
            MarketplaceInstallError::Sync {
                source_display,
                detail,
            } => {
                assert_eq!(source_display, "xAI Official");
                assert_eq!(detail, "network down");
            }
            other => panic!("expected Sync, got: {other}"),
        }
    }

    #[test]
    fn plan_install_qualifier_ok_selects_source_and_entry() {
        let sources = [
            local_source("Local Dev", "/tmp/p"),
            git_source("xAI Official", OFFICIAL_URL),
        ];
        let plan = plan_install(
            &sources,
            "SeNtRy",
            Some("xai-org/plugin-marketplace"),
            |_| Ok(vec![mp_entry("sentry")]),
        )
        .expect("resolves the official source");
        assert_eq!(plan.source_index, 1);
        assert_eq!(plan.entry.name, "sentry");
        assert_eq!(plan.entry.relative_path, "plugins/sentry");
        assert!(plan.other_copies_note.is_none());
        assert!(plan.skipped_sources.is_empty());
    }

    #[test]
    fn plan_install_bare_name_ambiguous_lists_candidate_labels() {
        let sources = [
            git_source("Third A", "https://github.com/acme/a.git"),
            git_source("Third B", "https://github.com/acme/b.git"),
        ];
        let err = plan_install(&sources, "sentry", None, |_| Ok(vec![mp_entry("sentry")]))
            .expect_err("two non-official sources provide sentry");
        match err {
            MarketplaceInstallError::NameAmbiguous { name, candidates } => {
                assert_eq!(name, "sentry");
                assert_eq!(
                    candidates,
                    vec![
                        "Third A (pin: sentry@acme/a)".to_string(),
                        "Third B (pin: sentry@acme/b)".to_string(),
                    ]
                );
            }
            other => panic!("expected NameAmbiguous, got: {other}"),
        }
    }

    #[test]
    fn plan_install_bare_name_official_priority_selects_official_and_sets_note() {
        let sources = [
            git_source("Third Party", "https://github.com/acme/x.git"),
            git_source("xAI Official", OFFICIAL_URL),
        ];
        let plan = plan_install(&sources, "sentry", None, |_| Ok(vec![mp_entry("sentry")]))
            .expect("official source wins the tie");
        assert_eq!(plan.source_index, 1);
        assert_eq!(plan.entry.name, "sentry");
        let note = plan
            .other_copies_note
            .expect("note set when other copies exist");
        assert!(note.contains("also available from 1 other"), "{note}");
        assert!(note.contains("sentry@<qualifier>"), "{note}");
    }

    #[test]
    fn plan_install_bare_name_partial_scan_when_only_source_skipped() {
        let sources = [git_source("Flaky Remote", "https://github.com/acme/x.git")];
        let err = plan_install(&sources, "sentry", None, |_| Err("boom".to_string()))
            .expect_err("only source failed to sync");
        match err {
            MarketplaceInstallError::PartialScan {
                name,
                skipped_sources,
            } => {
                assert_eq!(name, "sentry");
                assert_eq!(skipped_sources, vec!["Flaky Remote".to_string()]);
            }
            other => panic!("expected PartialScan, got: {other}"),
        }
    }

    #[test]
    fn plan_install_bare_name_skip_blocks_non_official_selection() {
        let sources = [
            git_source("Flaky Remote", "https://github.com/acme/a.git"),
            git_source("Good Remote", "https://github.com/acme/b.git"),
        ];
        let err = plan_install(&sources, "sentry", None, |source| {
            if source.name == "Flaky Remote" {
                Err("sync failed".to_string())
            } else {
                Ok(vec![mp_entry("sentry")])
            }
        })
        .expect_err("a skipped source must block selecting a non-official match");
        match err {
            MarketplaceInstallError::PartialScan {
                name,
                skipped_sources,
            } => {
                assert_eq!(name, "sentry");
                assert_eq!(skipped_sources, vec!["Flaky Remote".to_string()]);
            }
            other => panic!("expected PartialScan, got: {other}"),
        }
    }

    #[test]
    fn plan_install_bare_name_official_match_proceeds_despite_skip() {
        let sources = [
            git_source("xAI Official", OFFICIAL_URL),
            git_source("Flaky Remote", "https://github.com/acme/a.git"),
        ];
        let plan = plan_install(&sources, "sentry", None, |source| {
            if source.name == "xAI Official" {
                Ok(vec![mp_entry("sentry")])
            } else {
                Err("sync failed".to_string())
            }
        })
        .expect("official match is decisive even when another source is skipped");
        assert_eq!(plan.source_index, 0);
        assert_eq!(plan.entry.name, "sentry");
        assert_eq!(plan.skipped_sources, vec!["Flaky Remote".to_string()]);
    }

    #[test]
    fn plan_install_bare_name_local_winner_with_skip_is_partial_scan() {
        let sources = [
            local_source("Local Dev", "/tmp/p"),
            git_source("Flaky Remote", "https://github.com/acme/a.git"),
        ];
        let err = plan_install(&sources, "sentry", None, |source| {
            if matches!(&source.kind, SourceKind::Local { .. }) {
                Ok(vec![mp_entry("sentry")])
            } else {
                Err("sync failed".to_string())
            }
        })
        .expect_err("a skipped source blocks a non-official local winner");
        match err {
            MarketplaceInstallError::PartialScan {
                name,
                skipped_sources,
            } => {
                assert_eq!(name, "sentry");
                assert_eq!(skipped_sources, vec!["Flaky Remote".to_string()]);
            }
            other => panic!("expected PartialScan, got: {other}"),
        }
    }

    #[test]
    fn plan_install_bare_name_ambiguous_with_skip_is_partial_scan() {
        let sources = [
            git_source("Third A", "https://github.com/acme/a.git"),
            git_source("Third B", "https://github.com/acme/b.git"),
            git_source("Flaky Remote", "https://github.com/acme/c.git"),
        ];
        let err = plan_install(&sources, "sentry", None, |source| {
            if source.name == "Flaky Remote" {
                Err("sync failed".to_string())
            } else {
                Ok(vec![mp_entry("sentry")])
            }
        })
        .expect_err("ambiguous matches under a partial scan must fail closed");
        match err {
            MarketplaceInstallError::PartialScan {
                name,
                skipped_sources,
            } => {
                assert_eq!(name, "sentry");
                assert_eq!(skipped_sources, vec!["Flaky Remote".to_string()]);
            }
            other => panic!("expected PartialScan, got: {other}"),
        }
    }

    fn marketplace_allowlist(
        urls: &[&str],
    ) -> xai_grok_workspace::permission::resolution::MarketplaceAllowlist {
        xai_grok_workspace::permission::resolution::MarketplaceAllowlist {
            allowed_urls: urls.iter().map(|u| u.to_string()).collect(),
            source_path: None,
        }
    }

    #[test]
    fn filter_sources_by_allowlist_drops_blocked_git_keeps_allowed_and_local() {
        let allowlist = marketplace_allowlist(&["https://github.com/ok/repo.git"]);
        let sources = vec![
            git_source("Allowed", "https://github.com/ok/repo.git"),
            git_source("Blocked", "https://github.com/bad/repo.git"),
            local_source("Local", "/tmp/p"),
        ];
        let filtered = filter_sources_by_allowlist(sources, &allowlist);
        let names: Vec<&str> = filtered.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["Allowed", "Local"]);
    }

    #[test]
    fn filter_sources_by_allowlist_unrestricted_passes_everything() {
        let allowlist = xai_grok_workspace::permission::resolution::MarketplaceAllowlist::default();
        let sources = vec![
            git_source("Any Git", "https://github.com/bad/repo.git"),
            local_source("Local", "/tmp/p"),
        ];
        let filtered = filter_sources_by_allowlist(sources, &allowlist);
        let names: Vec<&str> = filtered.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["Any Git", "Local"]);
    }

    fn write_marketplace_plugin(marketplace: &Path, name: &str, version: &str) {
        let plugin_dir = marketplace.join("plugins").join(name);
        let manifest_dir = plugin_dir.join(".claude-plugin");
        std::fs::create_dir_all(&manifest_dir).unwrap();
        std::fs::write(
            manifest_dir.join("plugin.json"),
            format!(r#"{{"name":"{name}","version":"{version}"}}"#),
        )
        .unwrap();
        let skill_dir = plugin_dir.join("skills").join("demo");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "# Demo").unwrap();
    }

    #[test]
    fn install_marketplace_plugin_with_local_installs_then_short_circuits() {
        let marketplace = tempfile::tempdir().unwrap();
        write_marketplace_plugin(marketplace.path(), "demo", "1.0.0");
        let install_dir = tempfile::tempdir().unwrap();
        let cache_root = tempfile::tempdir().unwrap();
        let mut registry = InstallRegistry::empty(install_dir.path().to_path_buf());
        let sources = vec![local_source(
            "Local Dev",
            marketplace.path().to_str().unwrap(),
        )];
        let no_post = |_: &str| (Vec::new(), Vec::new());

        let outcome = install_marketplace_plugin_with(
            &sources,
            &mut registry,
            cache_root.path(),
            "demo",
            None,
            no_post,
        )
        .expect("local marketplace install should succeed");

        assert!(!outcome.already_installed);
        assert!(!outcome.source_is_git);
        assert_eq!(outcome.plugin_subdir, "plugins/demo");
        let repo = registry.get_repo(&outcome.repo_key).expect("repo recorded");
        assert!(
            matches!(repo.kind, InstallKind::Local { .. }),
            "local entry must install via the local (non-remote_url) fork"
        );
        let provenance = repo
            .marketplace
            .as_ref()
            .expect("marketplace provenance recorded");
        assert_eq!(
            provenance.source_url_or_path,
            marketplace.path().display().to_string()
        );
        assert_eq!(provenance.plugin_subdir, "plugins/demo");
        assert!(repo.plugins.contains_key("demo"));
        assert_eq!(registry.list().len(), 1);

        let outcome2 = install_marketplace_plugin_with(
            &sources,
            &mut registry,
            cache_root.path(),
            "demo",
            None,
            no_post,
        )
        .expect("second install should short-circuit");
        assert!(outcome2.already_installed);
        assert_eq!(outcome2.repo_key, outcome.repo_key);
        assert!(
            outcome2.plugin_names.contains(&"demo".to_string()),
            "already-installed outcome must carry the real installed plugin name for the update hint"
        );
        assert_eq!(
            registry.list().len(),
            1,
            "already-installed short-circuit must not create a duplicate repo"
        );
    }

    #[test]
    fn resolve_marketplace_source_name_with_local_returns_display_name() {
        let marketplace = tempfile::tempdir().unwrap();
        write_marketplace_plugin(marketplace.path(), "demo", "1.0.0");
        let cache_root = tempfile::tempdir().unwrap();
        let sources = vec![local_source(
            "Local Dev",
            marketplace.path().to_str().unwrap(),
        )];

        let name = resolve_marketplace_source_name_with(&sources, cache_root.path(), "demo", None)
            .expect("bare name should resolve to the local source");
        assert_eq!(name, "Local Dev");
    }

    #[test]
    fn resolve_qualified_source_name_with_matches_git_owner_repo() {
        let sources = vec![
            git_source(
                "xAI Official",
                "https://github.com/xai-org/plugin-marketplace.git",
            ),
            git_source(
                "Internal",
                "https://github.com/example/plugin-marketplace-internal.git",
            ),
        ];
        let name = resolve_qualified_source_name_with(&sources, "xai-org/plugin-marketplace")
            .expect("qualifier should match the official source");
        assert_eq!(name, "xAI Official");
    }

    #[test]
    fn resolve_qualified_source_name_with_matches_marketplace_name() {
        let sources = vec![git_source(
            "internal-tools",
            "git@github.example.com:acme/internal-tools.git",
        )];
        let name = resolve_qualified_source_name_with(&sources, "internal-tools")
            .expect("marketplace name should resolve for a GitHub Enterprise source");
        assert_eq!(name, "internal-tools");
    }

    #[test]
    fn resolve_qualified_source_name_with_unknown_qualifier_errors() {
        let sources = vec![git_source(
            "xAI Official",
            "https://github.com/xai-org/plugin-marketplace.git",
        )];
        let err = resolve_qualified_source_name_with(&sources, "bogus/repo")
            .expect_err("unknown qualifier should error");
        assert!(matches!(
            err,
            MarketplaceInstallError::UnknownQualifier { .. }
        ));
    }
}
