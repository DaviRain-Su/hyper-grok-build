//! `x.ai/marketplace/*` extension handlers.
//!
//! Provides marketplace browsing and install endpoints for the pager modal.
//! Delegates to `xai-grok-plugin-marketplace` crate for scanning and install logic.

use agent_client_protocol as acp;
use xai_hooks_plugins_types::{
    MarketplaceAction, MarketplaceActionRequest, MarketplaceListResponse, MarketplacePluginEntry,
    MarketplaceScanResult,
};

use crate::agent::MvpAgent;

type ExtResult = Result<acp::ExtResponse, acp::Error>;

fn load_filtered_marketplace_sources() -> Vec<xai_grok_plugin_marketplace::MarketplaceSource> {
    crate::plugin::load_filtered_marketplace_sources()
}

pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    match args.method.as_ref() {
        "x.ai/marketplace/list" => handle_list().await,
        "x.ai/marketplace/action" => handle_action(agent, args).await,
        _ => Err(acp::Error::method_not_found()),
    }
}

async fn handle_list() -> ExtResult {
    let t0 = std::time::Instant::now();
    let sources = load_filtered_marketplace_sources();
    let source_names: Vec<String> = sources
        .iter()
        .map(|s| {
            let url = match &s.kind {
                xai_grok_plugin_marketplace::SourceKind::Git { url, .. } => url.as_str(),
                xai_grok_plugin_marketplace::SourceKind::Local { path } => {
                    path.to_str().unwrap_or("?")
                }
            };
            format!("{}={}", s.name, url)
        })
        .collect();
    xai_grok_telemetry::unified_log::info(
        "marketplace handle_list: sources loaded",
        None,
        Some(serde_json::json!({
            "source_count": sources.len(),
            "sources": source_names,
            "load_sources_ms": t0.elapsed().as_millis() as u64,
        })),
    );

    // Scan all sources in parallel using blocking tasks (git operations are sync).
    let scan_handles: Vec<_> = sources
        .iter()
        .map(|source| {
            let source = source.clone();
            tokio::task::spawn_blocking(move || scan_source(&source))
        })
        .collect();
    let mut results = Vec::with_capacity(scan_handles.len());
    for (i, handle) in scan_handles.into_iter().enumerate() {
        let (scan, catalog_loaded) = handle.await.unwrap_or_else(|e| {
            (
                MarketplaceScanResult {
                    source_name: sources[i].name.clone(),
                    source_kind: String::new(),
                    source_url_or_path: String::new(),
                    plugins: Vec::new(),
                    error: Some(format!("scan task failed: {e}")),
                },
                false,
            )
        });
        let components_present = scan
            .plugins
            .iter()
            .filter(|p| p.components.is_some())
            .count();
        xai_grok_telemetry::unified_log::info(
            "marketplace handle_list: source scanned",
            None,
            Some(serde_json::json!({
                "source_index": i,
                "source_name": sources[i].name,
                "scan_ms": 0, // individual timing not available in parallel mode
                "plugin_count": scan.plugins.len(),
                "catalog_loaded": catalog_loaded,
                "components_present": components_present,
                "components_absent": scan.plugins.len() - components_present,
                "error": scan.error,
            })),
        );
        results.push(scan);
    }

    xai_grok_telemetry::unified_log::info(
        "marketplace handle_list: complete",
        None,
        Some(serde_json::json!({
            "total_ms": t0.elapsed().as_millis() as u64,
        })),
    );

    let response = MarketplaceListResponse { sources: results };
    super::to_ext_response(Ok(response))
}

async fn handle_action(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req: MarketplaceActionRequest = super::parse_params(args)?;
    let sid = acp::SessionId::new(req.session_id);

    let outcome = match req.action {
        MarketplaceAction::Refresh { source_url_or_path } => {
            // Force re-sync git caches; local sources are re-scanned on next list.
            let sources = load_filtered_marketplace_sources();
            let mut refreshed = 0;
            let mut errors = Vec::new();
            for source in &sources {
                if let Some(ref filter) = source_url_or_path {
                    let identity = match &source.kind {
                        xai_grok_plugin_marketplace::SourceKind::Local { path } => {
                            path.display().to_string()
                        }
                        xai_grok_plugin_marketplace::SourceKind::Git { url, .. } => url.clone(),
                    };
                    if &identity != filter {
                        continue;
                    }
                }
                if let xai_grok_plugin_marketplace::SourceKind::Git { url, branch } = &source.kind {
                    let cache_root = xai_grok_plugin_marketplace::git::default_cache_root();
                    if let Err(e) = xai_grok_plugin_marketplace::git::force_sync_source_cache(
                        url,
                        branch.as_deref(),
                        &cache_root,
                    ) {
                        errors.push(format!("{}: {e}", source.name));
                    }
                }
                refreshed += 1;
            }

            let msg = if errors.is_empty() {
                format!("Refreshed {refreshed} source(s).")
            } else {
                format!(
                    "Refreshed {refreshed} source(s) with {} error(s): {}",
                    errors.len(),
                    errors.join("; ")
                )
            };
            xai_hooks_plugins_types::ActionOutcome {
                status: xai_hooks_plugins_types::OutcomeStatus::Success,
                message: msg,
                requires_reload: false,
                requires_restart: false,
            }
        }
        MarketplaceAction::Install {
            source_url_or_path,
            plugin_relative_path,
        } => handle_install(agent, &sid, &source_url_or_path, &plugin_relative_path).await,
        MarketplaceAction::Update {
            source_url_or_path,
            plugin_relative_path,
        } => handle_update(agent, &sid, &source_url_or_path, &plugin_relative_path).await,
        MarketplaceAction::Uninstall {
            source_url_or_path,
            plugin_relative_path,
        } => handle_uninstall(agent, &sid, &source_url_or_path, &plugin_relative_path).await,
        MarketplaceAction::AddSource { url } => handle_add_source(&url).await,
        MarketplaceAction::RemoveSource { source_url_or_path } => {
            handle_remove_source(&source_url_or_path).await
        }
    };

    super::to_ext_response(Ok(outcome))
}

async fn handle_update(
    agent: &MvpAgent,
    sid: &acp::SessionId,
    source_url_or_path: &str,
    plugin_relative_path: &str,
) -> xai_hooks_plugins_types::ActionOutcome {
    use xai_grok_plugin_marketplace::installer;
    use xai_hooks_plugins_types::{ActionOutcome, OutcomeStatus};

    let sources = load_filtered_marketplace_sources();

    let source_identity = |s: &xai_grok_plugin_marketplace::MarketplaceSource| -> String {
        match &s.kind {
            xai_grok_plugin_marketplace::SourceKind::Local { path } => path.display().to_string(),
            xai_grok_plugin_marketplace::SourceKind::Git { url, .. } => url.clone(),
        }
    };

    let source = match sources
        .iter()
        .find(|s| source_identity(s) == source_url_or_path)
    {
        Some(s) => s,
        None => {
            return ActionOutcome {
                status: OutcomeStatus::NotFound,
                message: format!("Marketplace source not found: {source_url_or_path}"),
                requires_reload: false,
                requires_restart: false,
            };
        }
    };

    let plugin_path =
        match xai_grok_plugin_marketplace::MarketplaceRelativePath::parse(plugin_relative_path) {
            Ok(path) => path,
            Err(e) => {
                return ActionOutcome {
                    status: OutcomeStatus::ValidationError,
                    message: format!("Invalid plugin path: {e}"),
                    requires_reload: false,
                    requires_restart: false,
                };
            }
        };
    let plugin_relative_path = plugin_path.as_str();

    let marketplace_lease;
    let marketplace_root = match &source.kind {
        xai_grok_plugin_marketplace::SourceKind::Local { path } => {
            marketplace_lease = None;
            path.clone()
        }
        xai_grok_plugin_marketplace::SourceKind::Git { url, branch } => {
            let cache_root = xai_grok_plugin_marketplace::git::default_cache_root();
            match xai_grok_plugin_marketplace::git::sync_source_cache_with_mode(
                url,
                branch.as_deref(),
                &cache_root,
                xai_grok_plugin_marketplace::git::SyncMode::Force,
            ) {
                Ok(lease) => {
                    let path = lease.path.clone();
                    marketplace_lease = Some(lease);
                    path
                }
                Err(e) => {
                    return ActionOutcome {
                        status: OutcomeStatus::InternalError,
                        message: format!("Git sync failed: {e}"),
                        requires_reload: false,
                        requires_restart: false,
                    };
                }
            }
        }
    };

    let scan = xai_grok_plugin_marketplace::scan_marketplace(&marketplace_root);
    let entry = match scan
        .entries
        .into_iter()
        .find(|entry| entry.relative_path == plugin_relative_path)
    {
        Some(entry) => entry,
        None => {
            return ActionOutcome {
                status: OutcomeStatus::NotFound,
                message: format!("Marketplace plugin not found: {plugin_relative_path}"),
                requires_reload: false,
                requires_restart: false,
            };
        }
    };

    let provenance = xai_grok_agent::plugins::install_registry::MarketplaceProvenance {
        source_url_or_path: source_url_or_path.to_string(),
        source_display_name: source.name.clone(),
        plugin_subdir: plugin_relative_path.to_string(),
    };
    let mut registry = xai_grok_agent::plugins::install_registry::InstallRegistry::load();
    let require_sha = crate::plugin::marketplace_require_sha();
    let update_result = installer::update_from_marketplace_entry_transactional(
        &marketplace_root,
        &entry,
        provenance,
        &mut registry,
        require_sha,
    );
    drop(marketplace_lease);

    match update_result {
        Ok(result) => {
            let (_, post_warnings) = crate::config::post_install_plugin(&result.repo_key);
            for w in &post_warnings {
                tracing::warn!("{w}");
            }
            let reload_outcome = agent
                .execute_plugins_action(sid, xai_hooks_plugins_types::PluginsAction::Reload)
                .await;
            let mut msg = format!(
                "Updated {} ({} -> {})",
                result.repo_key,
                result.old_version.as_deref().unwrap_or("?"),
                result.new_version.as_deref().unwrap_or("?")
            );
            if reload_outcome.is_none() {
                msg.push_str("\nRestart or run /plugins reload to activate the update.");
            }
            ActionOutcome {
                status: OutcomeStatus::Success,
                message: msg,
                requires_reload: false,
                requires_restart: false,
            }
        }
        Err(xai_grok_agent::plugins::install_registry::InstallError::PluginNotFound { .. }) => {
            ActionOutcome {
                status: OutcomeStatus::NotFound,
                message: format!(
                    "Plugin not installed from this marketplace source: {plugin_relative_path}"
                ),
                requires_reload: false,
                requires_restart: false,
            }
        }
        Err(e) => ActionOutcome {
            status: OutcomeStatus::InternalError,
            message: format!("Update failed: {e}"),
            requires_reload: false,
            requires_restart: false,
        },
    }
}
async fn handle_install(
    agent: &MvpAgent,
    sid: &acp::SessionId,
    source_url_or_path: &str,
    plugin_relative_path: &str,
) -> xai_hooks_plugins_types::ActionOutcome {
    use xai_grok_plugin_marketplace::installer;
    use xai_hooks_plugins_types::{ActionOutcome, OutcomeStatus};

    let sources = load_filtered_marketplace_sources();

    // Helper: get canonical URL/path for a source.
    let source_identity = |s: &xai_grok_plugin_marketplace::MarketplaceSource| -> String {
        match &s.kind {
            xai_grok_plugin_marketplace::SourceKind::Local { path } => path.display().to_string(),
            xai_grok_plugin_marketplace::SourceKind::Git { url, .. } => url.clone(),
        }
    };

    let source = match sources
        .iter()
        .find(|s| source_identity(s) == source_url_or_path)
    {
        Some(s) => s,
        None => {
            return ActionOutcome {
                status: OutcomeStatus::NotFound,
                message: format!("Marketplace source not found: {source_url_or_path}"),
                requires_reload: false,
                requires_restart: false,
            };
        }
    };

    // Check if this is a URL-sourced plugin by scanning the marketplace index.
    let (scan, _) = scan_source(source);
    let remote_entry = scan
        .plugins
        .iter()
        .find(|p| p.relative_path == plugin_relative_path)
        .and_then(|p| {
            p.remote_url.as_ref().map(|url| {
                (
                    url.clone(),
                    p.remote_ref.clone(),
                    p.remote_sha.clone(),
                    p.remote_subdir.clone(),
                )
            })
        });

    if let Some((remote_url, remote_ref, remote_sha, remote_subdir)) = remote_entry {
        // URL-sourced plugin: clone from remote git URL.
        let provenance = xai_grok_agent::plugins::install_registry::MarketplaceProvenance {
            source_url_or_path: source_url_or_path.to_string(),
            source_display_name: source.name.clone(),
            plugin_subdir: plugin_relative_path.to_string(),
        };
        let mut registry = xai_grok_agent::plugins::install_registry::InstallRegistry::load();
        let require_sha = crate::plugin::marketplace_require_sha();
        match installer::install_from_remote_url(
            &remote_url,
            remote_ref.as_deref(),
            remote_sha.as_deref(),
            remote_subdir.as_deref(),
            plugin_relative_path,
            provenance,
            &mut registry,
            require_sha,
        ) {
            Ok(installer::MarketplaceInstallResult::Installed { repo_key }) => {
                // Auto-enable installed plugin so it's active after reload.
                let (_, post_warnings) = crate::config::post_install_plugin(&repo_key);
                for w in &post_warnings {
                    tracing::warn!("{w}");
                }
                let _ = agent
                    .execute_plugins_action(sid, xai_hooks_plugins_types::PluginsAction::Reload)
                    .await;
                ActionOutcome {
                    status: OutcomeStatus::Success,
                    message: format!(
                        "Installed from {}: {plugin_relative_path} (key: {repo_key})",
                        source.name,
                    ),
                    requires_reload: false,
                    requires_restart: false,
                }
            }
            Ok(installer::MarketplaceInstallResult::AlreadyInstalled { repo_key }) => {
                ActionOutcome {
                    status: OutcomeStatus::ValidationError,
                    message: format!(
                        "Already installed (key: {repo_key}). Use Update to reinstall."
                    ),
                    requires_reload: false,
                    requires_restart: false,
                }
            }
            Err(e) => ActionOutcome {
                status: OutcomeStatus::InternalError,
                message: format!("Install failed: {e}"),
                requires_reload: false,
                requires_restart: false,
            },
        }
    } else {
        // Local-sourced plugin: resolve from marketplace directory.
        let marketplace_lease;
        let marketplace_root = match &source.kind {
            xai_grok_plugin_marketplace::SourceKind::Local { path } => {
                marketplace_lease = None;
                path.clone()
            }
            xai_grok_plugin_marketplace::SourceKind::Git { url, branch } => {
                let cache_root = xai_grok_plugin_marketplace::git::default_cache_root();
                match xai_grok_plugin_marketplace::git::sync_source_cache_with_mode(
                    url,
                    branch.as_deref(),
                    &cache_root,
                    xai_grok_plugin_marketplace::git::SyncMode::UseTtl,
                ) {
                    Ok(lease) => {
                        let cached_path = lease.path.clone();
                        marketplace_lease = Some(lease);
                        cached_path
                    }
                    Err(e) => {
                        return ActionOutcome {
                            status: OutcomeStatus::InternalError,
                            message: format!("Git sync failed: {e}"),
                            requires_reload: false,
                            requires_restart: false,
                        };
                    }
                }
            }
        };

        let plugin_path =
            match xai_grok_plugin_marketplace::MarketplaceRelativePath::parse(plugin_relative_path)
            {
                Ok(path) => path,
                Err(e) => {
                    return ActionOutcome {
                        status: OutcomeStatus::ValidationError,
                        message: format!("Invalid plugin path: {e}"),
                        requires_reload: false,
                        requires_restart: false,
                    };
                }
            };
        let plugin_dir = match plugin_path.join_under(&marketplace_root) {
            Ok(path) => path,
            Err(e) => {
                return ActionOutcome {
                    status: OutcomeStatus::ValidationError,
                    message: format!("Invalid plugin path: {e}"),
                    requires_reload: false,
                    requires_restart: false,
                };
            }
        };
        if !plugin_dir.is_dir() {
            return ActionOutcome {
                status: OutcomeStatus::NotFound,
                message: format!("Plugin directory not found: {}", plugin_dir.display()),
                requires_reload: false,
                requires_restart: false,
            };
        };
        let plugin_relative_path = plugin_path.as_str();

        let provenance = xai_grok_agent::plugins::install_registry::MarketplaceProvenance {
            source_url_or_path: source_url_or_path.to_string(),
            source_display_name: source.name.clone(),
            plugin_subdir: plugin_relative_path.to_string(),
        };

        let mut registry = xai_grok_agent::plugins::install_registry::InstallRegistry::load();
        let install_result = installer::install_from_marketplace(
            &marketplace_root,
            plugin_relative_path,
            provenance,
            &mut registry,
        );
        drop(marketplace_lease);
        match install_result {
            Ok(installer::MarketplaceInstallResult::Installed { repo_key }) => {
                // Auto-enable installed plugin so it's active after reload.
                let (_, post_warnings) = crate::config::post_install_plugin(&repo_key);
                for w in &post_warnings {
                    tracing::warn!("{w}");
                }
                let _ = agent
                    .execute_plugins_action(sid, xai_hooks_plugins_types::PluginsAction::Reload)
                    .await;
                ActionOutcome {
                    status: OutcomeStatus::Success,
                    message: format!(
                        "Installed from {}: {plugin_relative_path} (key: {repo_key})",
                        source.name,
                    ),
                    requires_reload: false,
                    requires_restart: false,
                }
            }
            Ok(installer::MarketplaceInstallResult::AlreadyInstalled { repo_key }) => {
                ActionOutcome {
                    status: OutcomeStatus::ValidationError,
                    message: format!(
                        "Already installed (key: {repo_key}). Use Update to reinstall."
                    ),
                    requires_reload: false,
                    requires_restart: false,
                }
            }
            Err(e) => ActionOutcome {
                status: OutcomeStatus::InternalError,
                message: format!("Install failed: {e}"),
                requires_reload: false,
                requires_restart: false,
            },
        }
    }
}

async fn handle_uninstall(
    agent: &MvpAgent,
    sid: &acp::SessionId,
    source_url_or_path: &str,
    plugin_relative_path: &str,
) -> xai_hooks_plugins_types::ActionOutcome {
    use xai_grok_plugin_marketplace::installer;
    use xai_hooks_plugins_types::{ActionOutcome, OutcomeStatus};

    let mut registry = xai_grok_agent::plugins::install_registry::InstallRegistry::load();

    // Find the installed entry by marketplace provenance.
    let found = installer::find_installed_marketplace_plugin(
        &registry,
        source_url_or_path,
        plugin_relative_path,
    );

    let repo_key = match found {
        Some((key, _version)) => key,
        None => {
            return ActionOutcome {
                status: OutcomeStatus::NotFound,
                message: format!("Plugin not installed from marketplace: {plugin_relative_path}"),
                requires_reload: false,
                requires_restart: false,
            };
        }
    };

    // Delete the installed plugin directory.
    let plugin_dir = registry.install_dir().join(&repo_key);
    if plugin_dir.is_dir()
        && let Err(e) = std::fs::remove_dir_all(&plugin_dir)
    {
        return ActionOutcome {
            status: OutcomeStatus::InternalError,
            message: format!("Failed to remove plugin directory: {e}"),
            requires_reload: false,
            requires_restart: false,
        };
    }

    // Remove from registry and save.
    registry.remove(&repo_key);
    if let Err(e) = registry.save() {
        return ActionOutcome {
            status: OutcomeStatus::InternalError,
            message: format!("Failed to save registry: {e}"),
            requires_reload: false,
            requires_restart: false,
        };
    }

    // Trigger plugin reload so the removed plugin disappears from the session.
    let _ = agent
        .execute_plugins_action(sid, xai_hooks_plugins_types::PluginsAction::Reload)
        .await;

    ActionOutcome {
        status: OutcomeStatus::Success,
        message: format!("Uninstalled {plugin_relative_path} (key: {repo_key})"),
        requires_reload: false,
        requires_restart: false,
    }
}

fn scan_source(
    source: &xai_grok_plugin_marketplace::MarketplaceSource,
) -> (MarketplaceScanResult, bool) {
    let lease;
    let (source_kind, source_url_or_path, root) = match &source.kind {
        xai_grok_plugin_marketplace::SourceKind::Local { path } => {
            lease = None;
            (
                "local".to_string(),
                path.display().to_string(),
                Some(path.clone()),
            )
        }
        xai_grok_plugin_marketplace::SourceKind::Git { url, branch } => {
            let cache_root = xai_grok_plugin_marketplace::git::default_cache_root();
            let t_git = std::time::Instant::now();
            match xai_grok_plugin_marketplace::git::sync_source_cache_with_mode(
                url,
                branch.as_deref(),
                &cache_root,
                xai_grok_plugin_marketplace::git::SyncMode::UseTtl,
            ) {
                Ok(cache_lease) => {
                    let cached_path = cache_lease.path.clone();
                    lease = Some(cache_lease);
                    xai_grok_telemetry::unified_log::info(
                        "scan_source: git sync done",
                        None,
                        Some(serde_json::json!({
                            "url": url,
                            "git_sync_ms": t_git.elapsed().as_millis() as u64,
                        })),
                    );
                    ("git".to_string(), url.clone(), Some(cached_path))
                }
                Err(e) => {
                    return (
                        MarketplaceScanResult {
                            source_name: source.name.clone(),
                            source_kind: "git".to_string(),
                            source_url_or_path: url.clone(),
                            plugins: Vec::new(),
                            error: Some(format!("Git sync failed: {e}")),
                        },
                        false,
                    );
                }
            }
        }
    };

    let root = match root {
        Some(r) if r.is_dir() => r,
        _ => {
            return (
                MarketplaceScanResult {
                    source_name: source.name.clone(),
                    source_kind,
                    source_url_or_path: source_url_or_path.clone(),
                    plugins: Vec::new(),
                    error: Some(format!("Directory not found: {source_url_or_path}")),
                },
                false,
            );
        }
    };

    let scan = xai_grok_plugin_marketplace::scan_marketplace(&root);
    let catalog_loaded = scan.catalog_loaded;
    let discovered = scan.entries;
    drop(lease);

    // Cross-reference with install registry.
    let registry = xai_grok_agent::plugins::install_registry::InstallRegistry::load();
    let plugins = discovered
        .into_iter()
        .map(|p| {
            let (install_status, installed_version) =
                match xai_grok_plugin_marketplace::installer::find_installed_marketplace_plugin(
                    &registry,
                    &source_url_or_path,
                    &p.relative_path,
                ) {
                    Some((_key, ver)) => {
                        if p.version.is_none()
                            || p.version.as_deref() == Some(ver.as_str())
                            || ver.is_empty()
                        {
                            ("installed".to_string(), Some(ver))
                        } else {
                            ("update_available".to_string(), Some(ver))
                        }
                    }
                    None => ("not_installed".to_string(), None),
                };

            to_plugin_entry(p, install_status, installed_version)
        })
        .collect();

    (
        MarketplaceScanResult {
            source_name: source.name.clone(),
            source_kind,
            source_url_or_path,
            plugins,
            error: None,
        },
        catalog_loaded,
    )
}

fn to_plugin_entry(
    p: xai_grok_plugin_marketplace::MarketplaceEntry,
    install_status: String,
    installed_version: Option<String>,
) -> MarketplacePluginEntry {
    MarketplacePluginEntry {
        name: p.name,
        version: p.version,
        description: p.description,
        category: p.category,
        author: p.author,
        tags: p.tags,
        keywords: p.keywords,
        domains: p.domains,
        homepage: p.homepage,
        relative_path: p.relative_path,
        skill_count: p.skill_count,
        has_hooks: p.has_hooks,
        has_agents: p.has_agents,
        has_mcp: p.has_mcp,
        install_status,
        installed_version,
        components: p.components,
        remote_url: p.remote_url,
        remote_ref: p.remote_ref,
        remote_sha: p.remote_sha,
        remote_subdir: p.remote_subdir,
    }
}

/// Add a new git or local-path marketplace source to `~/.grok/config.toml`.
async fn handle_add_source(url: &str) -> xai_hooks_plugins_types::ActionOutcome {
    use crate::plugin::{self, MarketplaceAddInput};
    use xai_hooks_plugins_types::{ActionOutcome, OutcomeStatus};

    let url = url.trim();
    if url.is_empty() {
        return ActionOutcome {
            status: OutcomeStatus::ValidationError,
            message: "URL cannot be empty.".into(),
            requires_reload: false,
            requires_restart: false,
        };
    }

    let cwd = std::env::current_dir().unwrap_or_default();
    let input = plugin::classify_marketplace_add_input(url, &cwd);

    // Fail fast on missing local paths: without this, a path input would be
    // stored as a git URL and only error after network clone attempts.
    if let MarketplaceAddInput::LocalPath(path) = &input
        && !path.is_dir()
    {
        return ActionOutcome {
            status: OutcomeStatus::ValidationError,
            message: format!(
                "Local marketplace path not found (or is not a directory): {}",
                path.display()
            ),
            requires_reload: false,
            requires_restart: false,
        };
    }

    let identity = match &input {
        MarketplaceAddInput::GitUrl(u) => u.clone(),
        MarketplaceAddInput::LocalPath(p) => p.display().to_string(),
    };

    // Local paths never match the git-URL allowlist, so a restricted
    // strictKnownMarketplaces policy blocks them — intentionally fail-closed.
    let allowlist =
        &xai_grok_workspace::permission::resolution::managed_settings().marketplace_allowlist;
    if allowlist.is_restricted() && !allowlist.is_url_allowed(&identity) {
        return ActionOutcome {
            status: OutcomeStatus::ValidationError,
            message: format!("Marketplace source blocked: {}", allowlist.block_reason()),
            requires_reload: false,
            requires_restart: false,
        };
    }

    let config = crate::config::load_effective_config()
        .ok()
        .unwrap_or(toml::Value::Table(toml::map::Map::new()));
    let existing = xai_grok_plugin_marketplace::load_sources(&config);
    let already_configured = match &input {
        MarketplaceAddInput::GitUrl(git_url) => {
            let normalized = git_url.trim_end_matches(".git");
            existing.iter().any(|s| {
                matches!(&s.kind, xai_grok_plugin_marketplace::SourceKind::Git { url: u, .. }
                    if u.trim_end_matches(".git") == normalized)
            })
        }
        MarketplaceAddInput::LocalPath(path) => existing.iter().any(|s| {
            matches!(&s.kind, xai_grok_plugin_marketplace::SourceKind::Local { path: p }
                if p == path)
        }),
    };
    if already_configured {
        return ActionOutcome {
            status: OutcomeStatus::ValidationError,
            message: format!("Marketplace source already configured: {identity}"),
            requires_reload: false,
            requires_restart: false,
        };
    }

    let is_official = matches!(&input, MarketplaceAddInput::GitUrl(u)
        if xai_grok_plugin_marketplace::is_official_source_url(u));
    let name = if is_official {
        xai_grok_plugin_marketplace::OFFICIAL_SOURCE_NAME.to_string()
    } else {
        match &input {
            MarketplaceAddInput::GitUrl(u) => plugin::name_from_url(u),
            MarketplaceAddInput::LocalPath(p) => plugin::name_from_path(p),
        }
    };

    // Run the write under SAVE_LOCK + flock, off the reactor.
    let config_path = xai_grok_config::grok_home().join("config.toml");
    let grok_home = xai_grok_config::grok_home();
    let _save_guard = crate::util::config::lock_config_writes().await;
    let write = {
        let name = name.clone();
        tokio::task::spawn_blocking(move || {
            let _flock = acquire_init_lock(&grok_home).ok();
            add_marketplace_source(&config_path, &name, &input, is_official)
        })
        .await
    };
    match write {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return ActionOutcome {
                status: OutcomeStatus::InternalError,
                message: format!("Failed to write config: {e}"),
                requires_reload: false,
                requires_restart: false,
            };
        }
        Err(e) => {
            return ActionOutcome {
                status: OutcomeStatus::InternalError,
                message: format!("Config write task failed: {e}"),
                requires_reload: false,
                requires_restart: false,
            };
        }
    }

    ActionOutcome {
        status: OutcomeStatus::Success,
        message: format!("Added marketplace source: {name} ({identity})"),
        requires_reload: true,
        requires_restart: false,
    }
}

/// Append a `[[marketplace.sources]]` entry (and optionally set the official
/// flag) in one atomic `toml_edit` write, so a crash can't leave a source
/// without its flag. Idempotent on normalized git URL / local path; preserves
/// comments.
fn add_marketplace_source(
    config_path: &std::path::Path,
    name: &str,
    source: &crate::plugin::MarketplaceAddInput,
    set_official_flag: bool,
) -> std::io::Result<()> {
    if let Some(parent) = config_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let existing = crate::util::config::read_to_string_or_empty(config_path)?;
    let mut doc = existing.parse::<toml_edit::DocumentMut>().map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid TOML: {e}"),
        )
    })?;

    let marketplace_item = doc
        .entry("marketplace")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    let marketplace = marketplace_item.as_table_mut().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "[marketplace] is not a table",
        )
    })?;

    let sources_item = marketplace
        .entry("sources")
        .or_insert_with(|| toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    let sources = sources_item.as_array_of_tables_mut().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "[[marketplace.sources]] is not an array of tables",
        )
    })?;

    // Skip if the normalized URL / path already exists: the pre-lock dup check
    // in handle_add_source can let two serialized adds reach here.
    use crate::plugin::MarketplaceAddInput;
    let already_present = match source {
        MarketplaceAddInput::GitUrl(git_url) => {
            let normalized = git_url.trim_end_matches(".git");
            sources.iter().any(|t| {
                t.get("git")
                    .and_then(|v| v.as_str())
                    .is_some_and(|u| u.trim_end_matches(".git") == normalized)
            })
        }
        MarketplaceAddInput::LocalPath(path) => {
            let path_str = path.display().to_string();
            sources.iter().any(|t| {
                t.get("path")
                    .and_then(|v| v.as_str())
                    .is_some_and(|p| p == path_str)
            })
        }
    };
    if !already_present {
        let mut entry = toml_edit::Table::new();
        entry["name"] = toml_edit::value(name.to_string());
        match source {
            MarketplaceAddInput::GitUrl(git_url) => {
                entry["git"] = toml_edit::value(git_url.to_string());
            }
            MarketplaceAddInput::LocalPath(path) => {
                entry["path"] = toml_edit::value(path.display().to_string());
            }
        }
        sources.push(entry);
    }

    if set_official_flag {
        marketplace["official_marketplace_auto_installed"] = toml_edit::value(true);
    }

    crate::util::config::atomic_write_string(config_path, &doc.to_string())
}

/// Remove a marketplace source from `~/.grok/config.toml` and uninstall all
/// plugins that were installed from it.
async fn handle_remove_source(source_url_or_path: &str) -> xai_hooks_plugins_types::ActionOutcome {
    let src = source_url_or_path.to_string();
    // Lock + run the blocking FS work off the reactor.
    let _save_guard = crate::util::config::lock_config_writes().await;
    match tokio::task::spawn_blocking(move || remove_source_locked(&src)).await {
        Ok(outcome) => outcome,
        Err(e) => xai_hooks_plugins_types::ActionOutcome {
            status: xai_hooks_plugins_types::OutcomeStatus::InternalError,
            message: format!("Config write task failed: {e}"),
            requires_reload: false,
            requires_restart: false,
        },
    }
}

/// Sync body of [`handle_remove_source`], run on a blocking thread under the
/// flock for the whole read-modify-write so a concurrent auto-register can't
/// re-add the source mid-removal.
fn remove_source_locked(source_url_or_path: &str) -> xai_hooks_plugins_types::ActionOutcome {
    use crate::plugin;
    use xai_hooks_plugins_types::{ActionOutcome, OutcomeStatus};

    let grok_home = xai_grok_config::grok_home();
    let _flock = acquire_init_lock(&grok_home).ok();

    let uninstalled = plugin::uninstall_marketplace_source_plugins(source_url_or_path);

    // Remove the source and (if official) set the flag in ONE atomic write so a
    // crash can't drop the flag and re-add the source next startup.
    let config_path = grok_home.join("config.toml");
    let is_official = xai_grok_plugin_marketplace::is_official_source_url(source_url_or_path);
    let mut removed_from_config = false;
    let content = match crate::util::config::read_to_string_or_empty(&config_path) {
        Ok(c) => c,
        Err(e) => {
            return ActionOutcome {
                status: OutcomeStatus::InternalError,
                message: format!("Failed to read config: {e}"),
                requires_reload: false,
                requires_restart: false,
            };
        }
    };
    if let Some(removed) = plugin::remove_toml_marketplace_block(&content, source_url_or_path) {
        let final_content = if is_official {
            match set_official_flag_in_toml(&removed) {
                Ok(c) => c,
                Err(e) => {
                    return ActionOutcome {
                        status: OutcomeStatus::InternalError,
                        message: format!("Failed to update config: {e}"),
                        requires_reload: false,
                        requires_restart: false,
                    };
                }
            }
        } else {
            removed
        };
        if let Err(e) = crate::util::config::atomic_write_string(&config_path, &final_content) {
            return ActionOutcome {
                status: OutcomeStatus::InternalError,
                message: format!("Failed to write config: {e}"),
                requires_reload: false,
                requires_restart: false,
            };
        }
        removed_from_config = true;
    }

    if !removed_from_config && !plugin::try_remove_source_from_json_files(source_url_or_path) {
        return ActionOutcome {
            status: OutcomeStatus::NotFound,
            message: format!("Source not found in config: {source_url_or_path}"),
            requires_reload: false,
            requires_restart: false,
        };
    }

    // JSON-store-only removal: set the flag separately (the config.toml path
    // already set it atomically above).
    if is_official
        && !removed_from_config
        && let Err(e) = set_official_marketplace_auto_installed(&config_path)
    {
        tracing::warn!(
            error = %e,
            path = %config_path.display(),
            "failed to set official_marketplace_auto_installed flag",
        );
    }

    let msg = if uninstalled.is_empty() {
        format!("Removed marketplace source: {source_url_or_path}")
    } else {
        format!(
            "Removed marketplace source and uninstalled {} plugin(s): {}",
            uninstalled.len(),
            uninstalled.join(", ")
        )
    };
    ActionOutcome {
        status: OutcomeStatus::Success,
        message: msg,
        requires_reload: true,
        requires_restart: false,
    }
}

fn set_marketplace_bool_flag_in_toml(content: &str, key: &str) -> std::io::Result<String> {
    let mut doc = content.parse::<toml_edit::DocumentMut>().map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid TOML: {e}"),
        )
    })?;

    let marketplace = doc
        .entry("marketplace")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "[marketplace] is not a table",
            )
        })?;
    marketplace[key] = toml_edit::value(true);

    Ok(doc.to_string())
}

fn set_marketplace_bool_flag(config_path: &std::path::Path, key: &str) -> std::io::Result<()> {
    if let Some(parent) = config_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let existing = crate::util::config::read_to_string_or_empty(config_path)?;
    let updated = set_marketplace_bool_flag_in_toml(&existing, key)?;
    crate::util::config::atomic_write_string(config_path, &updated)
}

fn read_marketplace_bool_flag(config_path: &std::path::Path, key: &str) -> bool {
    let raw = match std::fs::read_to_string(config_path) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let parsed: toml::Value = match toml::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return false,
    };
    parsed
        .get("marketplace")
        .and_then(|m| m.get(key))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn set_official_flag_in_toml(content: &str) -> std::io::Result<String> {
    set_marketplace_bool_flag_in_toml(content, "official_marketplace_auto_installed")
}

fn set_official_marketplace_auto_installed(config_path: &std::path::Path) -> std::io::Result<()> {
    set_marketplace_bool_flag(config_path, "official_marketplace_auto_installed")
}

fn read_official_marketplace_auto_installed(config_path: &std::path::Path) -> bool {
    read_marketplace_bool_flag(config_path, "official_marketplace_auto_installed")
}

/// Acquire an advisory exclusive `flock` on `<grok_home>/.config-init.lock`,
/// retrying briefly under contention, to serialize first-run auto-register
/// across processes. Only `WouldBlock` retries; other I/O errors return early.
/// The lock file is intentionally never removed (flock releases on exit).
fn acquire_init_lock(grok_home: &std::path::Path) -> std::io::Result<std::fs::File> {
    use fs2::FileExt;
    let _ = std::fs::create_dir_all(grok_home);
    let lock_path = grok_home.join(".config-init.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        // Contents are irrelevant; truncate(false) silences clippy::suspicious_open_options.
        .truncate(false)
        .open(&lock_path)?;
    for _ in 0..50 {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        format!("timed out waiting for {} after 1s", lock_path.display()),
    ))
}

fn is_default_skills_plugin_subdir(plugin_subdir: &str) -> bool {
    plugin_subdir == "default-skills"
}

fn default_skills_repo_keys<'a>(
    repos: impl IntoIterator<
        Item = (
            &'a str,
            &'a xai_grok_agent::plugins::install_registry::InstalledRepo,
        ),
    >,
) -> Vec<&'a str> {
    repos
        .into_iter()
        .filter_map(|(key, repo)| {
            repo.marketplace
                .as_ref()
                .filter(|mp| is_default_skills_plugin_subdir(&mp.plugin_subdir))
                .map(|_| key)
        })
        .collect()
}

fn set_default_skills_installs_purged(config_path: &std::path::Path) -> std::io::Result<()> {
    set_marketplace_bool_flag(config_path, "default_skills_installs_purged")
}

fn read_default_skills_installs_purged(config_path: &std::path::Path) -> bool {
    read_marketplace_bool_flag(config_path, "default_skills_installs_purged")
}

/// One-shot purge of legacy marketplace `default-skills` installs.
///
/// Gated by sticky `default_skills_installs_purged` in config.toml. Best-effort:
/// errors are logged and never block startup.
pub fn purge_default_skills_installs(grok_home: &std::path::Path) {
    purge_default_skills_installs_impl(grok_home, || {
        xai_grok_agent::plugins::install_registry::InstallRegistry::try_load_from(
            xai_grok_agent::plugins::install_registry::InstallRegistry::resolve_install_dir(),
        )
    });
}

fn purge_default_skills_installs_impl(
    grok_home: &std::path::Path,
    load_registry: impl FnOnce() -> Result<
        xai_grok_agent::plugins::install_registry::InstallRegistry,
        xai_grok_agent::plugins::install_registry::InstallError,
    >,
) {
    let config_path = grok_home.join("config.toml");

    if read_default_skills_installs_purged(&config_path) {
        return;
    }

    let _lock = match acquire_init_lock(grok_home) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %grok_home.join(".config-init.lock").display(),
                "skipping default-skills purge: failed to acquire init lock"
            );
            return;
        }
    };

    if read_default_skills_installs_purged(&config_path) {
        return;
    }

    let mut registry = match load_registry() {
        Ok(reg) => reg,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "skipping default-skills purge: failed to load install registry"
            );
            return;
        }
    };
    let keys: Vec<String> = default_skills_repo_keys(registry.list())
        .into_iter()
        .map(|k| k.to_string())
        .collect();

    for key in &keys {
        let path = registry
            .get_repo(key)
            .map(|r| r.path.clone())
            .unwrap_or_else(|| registry.install_dir().join(key));
        if path.exists()
            && let Err(e) = std::fs::remove_dir_all(&path)
        {
            let _ = std::fs::remove_file(&path);
            if path.exists() {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    repo_key = %key,
                    "failed to remove default-skills install dir"
                );
            }
        }
        registry.remove(key);
    }

    if !keys.is_empty() {
        if let Err(e) = registry.save() {
            tracing::warn!(error = %e, "failed to save registry after default-skills purge");
            return;
        }
        tracing::info!(
            count = keys.len(),
            "purged legacy default-skills marketplace installs"
        );
    }

    if let Err(e) = set_default_skills_installs_purged(&config_path) {
        tracing::warn!(
            error = %e,
            path = %config_path.display(),
            "failed to set default_skills_installs_purged flag"
        );
    }
}

/// Auto-register the official xAI marketplace source on first run.
///
/// Gated by the caller (`init_process`); see
/// `Config::resolve_official_marketplace_auto_register`. No-op once
/// `official_marketplace_auto_installed` is set. Under a process-wide flock it
/// adds the source (or just sets the flag if it's already present in config.toml
/// or a JSON store). Best-effort: errors are logged and never block startup.
pub fn ensure_official_marketplace_source(grok_home: &std::path::Path) {
    let config_path = grok_home.join("config.toml");

    if read_official_marketplace_auto_installed(&config_path) {
        return;
    }

    let _lock = match acquire_init_lock(grok_home) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %grok_home.join(".config-init.lock").display(),
                "skipping official marketplace auto-register: failed to acquire init lock"
            );
            return;
        }
    };

    // Re-check under the lock: another process may have registered meanwhile.
    if read_official_marketplace_auto_installed(&config_path) {
        return;
    }

    let raw = match crate::util::config::read_to_string_or_empty(&config_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "skipping official marketplace auto-register: cannot read config.toml");
            return;
        }
    };
    let parsed: toml::Value = match toml::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "skipping official marketplace auto-register: invalid config.toml");
            return;
        }
    };

    // "Already present" = official URL in config.toml sources OR a JSON store
    // (settings.json / known_marketplaces.json) under grok_home. Scoped to
    // grok_home only (not ~/.claude) to keep tests hermetic; a user with the URL
    // solely in ~/.claude gets one duplicate entry that the UI dedupes by URL.
    let toml_sources = xai_grok_plugin_marketplace::load_sources(&parsed);
    let json_sources = xai_grok_plugin_marketplace::load_extra_sources_from_settings_in(
        &toml_sources,
        std::slice::from_ref(&grok_home.to_path_buf()),
    );
    let already_present = toml_sources.iter().chain(json_sources.iter()).any(|s| {
        matches!(&s.kind, xai_grok_plugin_marketplace::SourceKind::Git { url, .. }
            if xai_grok_plugin_marketplace::is_official_source_url(url))
    });

    let write_result = if already_present {
        // Already present: just set the flag.
        set_official_marketplace_auto_installed(&config_path)
    } else {
        add_marketplace_source(
            &config_path,
            xai_grok_plugin_marketplace::OFFICIAL_SOURCE_NAME,
            &crate::plugin::MarketplaceAddInput::GitUrl(
                xai_grok_plugin_marketplace::OFFICIAL_SOURCE_GIT_URL.to_string(),
            ),
            true,
        )
    };

    match write_result {
        Ok(()) if !already_present => {
            tracing::info!(
                url = xai_grok_plugin_marketplace::OFFICIAL_SOURCE_GIT_URL,
                "auto-registered official xAI marketplace source"
            );
        }
        Ok(()) => {}
        Err(e) => {
            tracing::warn!(error = %e, "failed to auto-register official marketplace source");
        }
    }
}

#[cfg(test)]
mod official_source_tests {
    use super::*;

    fn read_sources(
        config_path: &std::path::Path,
    ) -> Vec<xai_grok_plugin_marketplace::MarketplaceSource> {
        let raw = std::fs::read_to_string(config_path).unwrap_or_default();
        let parsed: toml::Value =
            toml::from_str(&raw).unwrap_or_else(|_| toml::Value::Table(Default::default()));
        xai_grok_plugin_marketplace::load_sources(&parsed)
    }

    fn read_flag(config_path: &std::path::Path) -> bool {
        let raw = std::fs::read_to_string(config_path).unwrap_or_default();
        let parsed: toml::Value =
            toml::from_str(&raw).unwrap_or_else(|_| toml::Value::Table(Default::default()));
        parsed
            .get("marketplace")
            .and_then(|m| m.get("official_marketplace_auto_installed"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    #[test]
    fn set_official_flag_in_toml_preserves_other_content() {
        let content = "[ui]\ntheme = \"dark\"\n";
        let out = set_official_flag_in_toml(content).unwrap();
        assert!(out.contains("theme = \"dark\""), "{out}");
        assert!(
            out.contains("official_marketplace_auto_installed = true"),
            "{out}"
        );
    }

    #[test]
    fn add_marketplace_source_local_path_writes_path_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let dir = tmp.path().join("my-plugins");
        std::fs::create_dir_all(&dir).unwrap();

        let input = crate::plugin::MarketplaceAddInput::LocalPath(dir.clone());
        add_marketplace_source(&config_path, "my-plugins", &input, false).unwrap();
        // Idempotent on the same path.
        add_marketplace_source(&config_path, "my-plugins", &input, false).unwrap();

        let sources = read_sources(&config_path);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name, "my-plugins");
        assert!(matches!(
            &sources[0].kind,
            xai_grok_plugin_marketplace::SourceKind::Local { path }
if path == &dir
        ));
        // The path must not be mangled into a git URL.
        let raw = std::fs::read_to_string(&config_path).unwrap();
        assert!(!raw.contains("git ="), "{raw}");
    }

    #[test]
    fn removing_last_nonofficial_source_preserves_flag_and_blocks_readd() {
        // Regression: removing the last (non-official) source must not wipe the
        // sticky flag, or a gated startup would re-add the removed official source.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let config_path = home.join("config.toml");

        std::fs::write(
            &config_path,
            "[marketplace]\nofficial_marketplace_auto_installed = true\n\n\
             [[marketplace.sources]]\nname = \"custom\"\ngit = \"https://example.com/custom.git\"\n",
        )
        .unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let new_content = crate::plugin::remove_toml_marketplace_block(
            &content,
            "https://example.com/custom.git",
        )
        .expect("custom source should be removed");
        std::fs::write(&config_path, &new_content).unwrap();

        assert!(
            read_flag(&config_path),
            "flag must be preserved after removing the last source: {new_content}"
        );

        ensure_official_marketplace_source(home);
        let sources = read_sources(&config_path);
        assert!(
            !sources.iter().any(|s| matches!(&s.kind,
                xai_grok_plugin_marketplace::SourceKind::Git { url, .. }
                    if xai_grok_plugin_marketplace::is_official_source_url(url))),
            "official source must not be re-added after removal"
        );
    }

    #[test]
    fn first_run_creates_source_and_sets_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        ensure_official_marketplace_source(home);

        let config_path = home.join("config.toml");
        assert!(config_path.exists(), "config.toml should be created");

        let sources = read_sources(&config_path);
        assert_eq!(sources.len(), 1);
        assert_eq!(
            sources[0].name,
            xai_grok_plugin_marketplace::OFFICIAL_SOURCE_NAME
        );
        assert!(matches!(
            &sources[0].kind,
            xai_grok_plugin_marketplace::SourceKind::Git { url, .. }
                if url == xai_grok_plugin_marketplace::OFFICIAL_SOURCE_GIT_URL
        ));
        assert!(read_flag(&config_path));
    }

    #[test]
    fn second_run_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        ensure_official_marketplace_source(home);
        let after_first = std::fs::read_to_string(home.join("config.toml")).unwrap();

        ensure_official_marketplace_source(home);
        let after_second = std::fs::read_to_string(home.join("config.toml")).unwrap();

        assert_eq!(
            after_first, after_second,
            "second run must not modify config"
        );
    }

    #[test]
    fn removed_source_stays_removed_across_restarts() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let config_path = home.join("config.toml");

        ensure_official_marketplace_source(home);
        assert_eq!(read_sources(&config_path).len(), 1);

        // Simulate removal: drop the source block, keep the flag.
        std::fs::write(
            &config_path,
            "[marketplace]\nofficial_marketplace_auto_installed = true\n",
        )
        .unwrap();

        ensure_official_marketplace_source(home);

        assert!(
            read_sources(&config_path).is_empty(),
            "official source must not be re-added after removal"
        );
        assert!(read_flag(&config_path));
    }

    #[test]
    fn pre_existing_official_source_just_sets_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let config_path = home.join("config.toml");

        // User added the official source manually (flag not set).
        std::fs::write(
            &config_path,
            format!(
                "[[marketplace.sources]]\nname = \"{}\"\ngit = \"{}\"\n",
                xai_grok_plugin_marketplace::OFFICIAL_SOURCE_NAME,
                xai_grok_plugin_marketplace::OFFICIAL_SOURCE_GIT_URL,
            ),
        )
        .unwrap();

        ensure_official_marketplace_source(home);

        let sources = read_sources(&config_path);
        assert_eq!(sources.len(), 1, "must not duplicate existing source");
        assert!(read_flag(&config_path));
    }

    #[test]
    fn pre_existing_official_source_in_known_marketplaces_skips_append() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let plugins_dir = home.join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("known_marketplaces.json"),
            format!(
                r#"{{"xai-official":{{"source":{{"source":"git","url":"{}"}}}}}}"#,
                xai_grok_plugin_marketplace::OFFICIAL_SOURCE_GIT_URL,
            ),
        )
        .unwrap();

        ensure_official_marketplace_source(home);

        let config_path = home.join("config.toml");
        assert!(
            read_sources(&config_path).is_empty(),
            "must not append to config.toml when the source is already present in known_marketplaces.json"
        );
        assert!(
            read_flag(&config_path),
            "must set the auto-installed flag so subsequent restarts skip the append check"
        );
    }

    #[test]
    fn pre_existing_official_source_in_extra_known_marketplaces_skips_append() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::write(
            home.join("settings.json"),
            format!(
                r#"{{"extraKnownMarketplaces":{{"xai-official":{{"source":{{"source":"git","url":"{}"}}}}}}}}"#,
                xai_grok_plugin_marketplace::OFFICIAL_SOURCE_GIT_URL,
            ),
        )
        .unwrap();

        ensure_official_marketplace_source(home);

        let config_path = home.join("config.toml");
        assert!(
            read_sources(&config_path).is_empty(),
            "must not append to config.toml when source is in extraKnownMarketplaces"
        );
        assert!(read_flag(&config_path));
    }

    #[test]
    fn pre_existing_official_source_with_branch_just_sets_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let config_path = home.join("config.toml");

        // Official source pinned to a non-main branch: dedup must match URL alone.
        std::fs::write(
            &config_path,
            format!(
                "[[marketplace.sources]]\nname = \"{}\"\ngit = \"{}\"\nbranch = \"some-branch\"\n",
                xai_grok_plugin_marketplace::OFFICIAL_SOURCE_NAME,
                xai_grok_plugin_marketplace::OFFICIAL_SOURCE_GIT_URL,
            ),
        )
        .unwrap();

        ensure_official_marketplace_source(home);

        let sources = read_sources(&config_path);
        assert_eq!(sources.len(), 1, "must not duplicate existing source");
        assert!(
            std::fs::read_to_string(&config_path)
                .unwrap()
                .contains("branch = \"some-branch\""),
            "branch override must survive registration"
        );
        assert!(read_flag(&config_path));
    }

    #[test]
    fn preserves_existing_user_sources_and_comments() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let config_path = home.join("config.toml");
        std::fs::write(
            &config_path,
            "# my custom marketplaces\n[[marketplace.sources]]\nname = \"Local\"\npath = \"/tmp/mine\"\n",
        )
        .unwrap();

        ensure_official_marketplace_source(home);

        let after = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            after.contains("# my custom marketplaces"),
            "comments preserved"
        );
        assert!(after.contains("Local"), "existing source preserved");
        let sources = read_sources(&config_path);
        assert_eq!(sources.len(), 2);
        assert!(read_flag(&config_path));
    }
}

#[cfg(test)]
mod default_skills_purge_tests {
    use super::*;
    use xai_grok_agent::plugins::install_registry::{
        InstallKind, InstallRegistry, InstalledRepo, MarketplaceProvenance, RepoPlugin,
    };

    fn repo_at(path: &std::path::Path, plugin_subdir: Option<&str>) -> InstalledRepo {
        InstalledRepo {
            kind: InstallKind::Local {
                source_path: path.to_path_buf(),
                subdir: None,
            },
            installed_at: String::new(),
            updated_at: String::new(),
            path: path.to_path_buf(),
            plugins: std::collections::HashMap::from([(
                "p".into(),
                RepoPlugin {
                    subdir: None,
                    version: None,
                },
            )]),
            marketplace: plugin_subdir.map(|subdir| MarketplaceProvenance {
                source_url_or_path: "https://example.com/market.git".into(),
                source_display_name: "Test".into(),
                plugin_subdir: subdir.into(),
            }),
        }
    }

    #[test]
    fn match_is_exact_plugin_subdir_only() {
        assert!(is_default_skills_plugin_subdir("default-skills"));
        assert!(!is_default_skills_plugin_subdir("plugins/default-skills"));
        assert!(!is_default_skills_plugin_subdir("default-skills/extra"));
        assert!(!is_default_skills_plugin_subdir("defaults-skills"));
        assert!(!is_default_skills_plugin_subdir(""));
    }

    #[test]
    fn collects_only_default_skills_repo_keys() {
        let default_skills = repo_at(std::path::Path::new("/tmp/ds"), Some("default-skills"));
        let other = repo_at(std::path::Path::new("/tmp/office"), Some("plugins/office"));
        let no_marketplace = repo_at(std::path::Path::new("/tmp/local"), None);

        let keys = default_skills_repo_keys([
            ("ds-aaaa", &default_skills),
            ("office-bbbb", &other),
            ("local-cccc", &no_marketplace),
        ]);
        assert_eq!(keys, vec!["ds-aaaa"]);
    }

    #[test]
    fn purged_flag_toml_preserves_other_content() {
        let content =
            "[ui]\ntheme = \"dark\"\n[marketplace]\nofficial_marketplace_auto_installed = true\n";
        let out =
            set_marketplace_bool_flag_in_toml(content, "default_skills_installs_purged").unwrap();
        assert!(out.contains("theme = \"dark\""), "{out}");
        assert!(
            out.contains("official_marketplace_auto_installed = true"),
            "{out}"
        );
        assert!(
            out.contains("default_skills_installs_purged = true"),
            "{out}"
        );
    }

    #[test]
    fn read_purged_flag_false_when_missing_or_wrong_type() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        assert!(!read_default_skills_installs_purged(&path));

        std::fs::write(
            &path,
            "[marketplace]\ndefault_skills_installs_purged = \"yes\"\n",
        )
        .unwrap();
        assert!(!read_default_skills_installs_purged(&path));

        std::fs::write(
            &path,
            "[marketplace]\ndefault_skills_installs_purged = true\n",
        )
        .unwrap();
        assert!(read_default_skills_installs_purged(&path));
    }

    #[test]
    fn purge_sets_flag_when_nothing_to_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let install_dir = home.join("installed-plugins");
        purge_default_skills_installs_impl(home, || {
            Ok(InstallRegistry::empty(install_dir.clone()))
        });
        let config_path = home.join("config.toml");
        assert!(read_default_skills_installs_purged(&config_path));

        let after_first = std::fs::read_to_string(&config_path).unwrap();
        purge_default_skills_installs_impl(home, || Ok(InstallRegistry::empty(install_dir)));
        let after_second = std::fs::read_to_string(&config_path).unwrap();
        assert_eq!(after_first, after_second);
    }

    #[test]
    fn purge_skips_flag_when_registry_load_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let install_dir = home.join("installed-plugins");
        std::fs::create_dir_all(&install_dir).unwrap();
        std::fs::write(install_dir.join("registry.json"), "{not-json").unwrap();

        purge_default_skills_installs_impl(home, || {
            InstallRegistry::try_load_from(install_dir.clone())
        });

        assert!(!read_default_skills_installs_purged(
            &home.join("config.toml")
        ));
    }

    #[test]
    fn purge_removes_default_skills_retains_others_and_sets_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let install_dir = home.join("installed-plugins");
        std::fs::create_dir_all(&install_dir).unwrap();

        let ds_path = install_dir.join("ds-aaaa");
        std::fs::create_dir_all(&ds_path).unwrap();
        std::fs::write(ds_path.join("marker"), "ds").unwrap();

        let other_path = install_dir.join("office-bbbb");
        std::fs::create_dir_all(&other_path).unwrap();
        std::fs::write(other_path.join("marker"), "office").unwrap();

        let mut registry = InstallRegistry::empty(install_dir.clone());
        registry.insert("ds-aaaa".into(), repo_at(&ds_path, Some("default-skills")));
        registry.insert(
            "office-bbbb".into(),
            repo_at(&other_path, Some("plugins/office")),
        );
        registry.save().unwrap();

        let install_dir_for_load = install_dir.clone();
        purge_default_skills_installs_impl(home, move || {
            InstallRegistry::try_load_from(install_dir_for_load)
        });

        assert!(
            !ds_path.exists(),
            "default-skills install dir must be removed"
        );
        assert!(other_path.exists(), "non-matching install must be retained");

        let reloaded = InstallRegistry::load_from(install_dir.clone());
        assert!(reloaded.get_repo("ds-aaaa").is_none());
        assert!(reloaded.get_repo("office-bbbb").is_some());

        let config_path = home.join("config.toml");
        assert!(read_default_skills_installs_purged(&config_path));

        let after_first = std::fs::read_to_string(&config_path).unwrap();
        let install_dir_for_reload = install_dir;
        purge_default_skills_installs_impl(home, move || {
            InstallRegistry::try_load_from(install_dir_for_reload)
        });
        let after_second = std::fs::read_to_string(&config_path).unwrap();
        assert_eq!(after_first, after_second);
        assert!(other_path.exists());
    }
}

#[cfg(test)]
mod conversion_tests {
    use super::*;

    #[test]
    fn to_plugin_entry_carries_homepage_and_keywords() {
        let entry = xai_grok_plugin_marketplace::MarketplaceEntry {
            name: "demo".into(),
            version: Some("1.0.0".into()),
            description: Some("demo".into()),
            category: Some("dev".into()),
            author: Some("xai".into()),
            tags: vec!["cli".into()],
            keywords: vec!["search".into(), "rank".into()],
            domains: vec!["example.com".into()],
            homepage: Some("https://example.com/demo".into()),
            relative_path: "plugins/demo".into(),
            skill_count: 2,
            has_hooks: true,
            has_agents: false,
            has_mcp: false,
            remote_url: None,
            remote_ref: None,
            remote_sha: None,
            remote_subdir: Some("plugins/acme".into()),
            components: Some(xai_hooks_plugins_types::PluginComponents {
                skills: vec![xai_hooks_plugins_types::ComponentItem::new(
                    "code-review",
                    Some("Review staged changes".to_string()),
                )],
                ..Default::default()
            }),
        };

        let dto = to_plugin_entry(entry, "not_installed".to_string(), None);

        assert_eq!(dto.homepage.as_deref(), Some("https://example.com/demo"));
        assert_eq!(dto.keywords, vec!["search".to_string(), "rank".to_string()]);
        assert_eq!(dto.domains, vec!["example.com".to_string()]);
        assert_eq!(dto.tags, vec!["cli".to_string()]);
        assert_eq!(dto.install_status, "not_installed");
        assert_eq!(dto.remote_subdir.as_deref(), Some("plugins/acme"));
        let components = dto.components.expect("components passed through");
        assert_eq!(components.skills.len(), 1);
        assert_eq!(components.skills[0].name, "code-review");
        assert_eq!(
            components.skills[0].description.as_deref(),
            Some("Review staged changes")
        );
    }
}
