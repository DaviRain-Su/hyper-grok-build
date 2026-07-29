use xai_grok_telemetry::events::SessionHarness;
use xai_grok_workspace::permission::Decision;

/// Permission-mode label for the `session.permission_mode_changed` span.
pub(crate) fn permission_mode_label(is_yolo: bool) -> &'static str {
    if is_yolo {
        "bypassPermissions"
    } else {
        "default"
    }
}

/// Telemetry `source` label for a permission [`Decision`] on the `tool.decision`
/// span. `is_yolo` collapses auto-approvals to `config`. `Decision::Allow`/`Ask`
/// carry no provenance, so a config/policy allow is indistinguishable from a
/// user click — report neutral `allowed` rather than guessing `user_temporary`.
pub(crate) fn permission_decision_source(decision: &Decision, is_yolo: bool) -> &'static str {
    match decision {
        Decision::PolicyDeny(_) => "config",
        Decision::Reject(_) => "user_reject",
        Decision::Cancelled => "user_abort",
        Decision::FollowupMessage(_) => "user_followup",
        Decision::Allow | Decision::Ask if is_yolo => "config",
        Decision::Allow | Decision::Ask => "allowed",
    }
}

/// Emit an `mcp.server_connection` span. `duration_ms` / `tool_count` /
/// `error_type` are status-specific; pass `None` when not applicable.
pub(crate) fn emit_mcp_connection_span(
    status: &str,
    server_name: &str,
    transport_type: &str,
    server_scope: &str,
    duration_ms: Option<i64>,
    tool_count: Option<i64>,
    error_type: Option<&str>,
) {
    let span = tracing::info_span!(
        "mcp.server_connection",
        status,
        server_name,
        transport_type,
        server_scope,
        duration_ms = tracing::field::Empty,
        tool_count = tracing::field::Empty,
        error_type = tracing::field::Empty,
    );
    if let Some(d) = duration_ms {
        span.record("duration_ms", d);
    }
    if let Some(t) = tool_count {
        span.record("tool_count", t);
    }
    if let Some(e) = error_type {
        span.record("error_type", e);
    }
    span.in_scope(|| {});
}

/// Provenance for `skill.activated`'s `skill_source`: project (under `cwd`),
/// user (under `$HOME`), else bundled. Paths are canonicalized (symlinked cwd
/// like macOS `/tmp` vs `/private/tmp`); when both roots match, the deepest
/// wins, tie (cwd == `$HOME`) → user.
pub(crate) fn skill_source_label(skill_path: &str, cwd: &str) -> &'static str {
    let canon = |p: &std::path::Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let p = canon(std::path::Path::new(skill_path));
    let depth_if_under =
        |base: std::path::PathBuf| p.starts_with(&base).then(|| base.components().count());
    let project = depth_if_under(canon(std::path::Path::new(cwd)));
    let user = crate::util::grok_home::grok_home()
        .parent()
        .and_then(|home| depth_if_under(canon(home)));
    match (project, user) {
        (Some(pd), Some(ud)) if pd > ud => "projectSettings",
        (Some(_), Some(_)) => "userSettings",
        (Some(_), None) => "projectSettings",
        (None, Some(_)) => "userSettings",
        (None, None) => "bundled",
    }
}

pub(crate) fn format_hook_name(spec: &xai_grok_hooks::config::HookSpec) -> String {
    let scope = spec.name.split(':').next().unwrap_or("unknown");
    match spec.configured_matcher.as_deref() {
        Some(m) if !m.is_empty() => format!("{scope}:{}:{}", spec.event, m.to_lowercase()),
        _ => format!("{scope}:{}", spec.event),
    }
}

/// Provenance for telemetry, mapped from the shared [`hook_origin`] classifier so
/// this and `/hooks` inspect can't diverge.
fn format_hook_source(spec: &xai_grok_hooks::config::HookSpec) -> &'static str {
    use xai_grok_hooks::config::HookOrigin as O;
    match xai_grok_hooks::config::hook_origin(spec) {
        O::SystemManaged | O::Managed => "managedConfig",
        O::Requirements => "requirementsConfig",
        O::UserConfig => "userConfig",
        O::UserFile => "userSettings",
        O::ProjectFile => "projectSettings",
        O::Plugin => "pluginHook",
        O::Agent => "agentHook",
        O::Unknown => "unknown",
    }
}

/// Per-hook inventory recorded as a `hook.registered` span at session start.
pub(crate) struct HookRegInfo {
    pub name: String,
    pub event: String,
    pub hook_type: String,
    pub source: &'static str,
}

impl HookRegInfo {
    pub(crate) fn from_spec(spec: &xai_grok_hooks::config::HookSpec) -> Self {
        Self {
            name: format_hook_name(spec),
            event: spec.event.to_string(),
            hook_type: spec.handler_type.as_str().to_string(),
            source: format_hook_source(spec),
        }
    }
}

#[derive(Debug)]
pub(crate) struct SessionHarnessMetrics {
    pub session_id: String,
    pub client_identifier: Option<String>,
    pub model_id: String,
    pub agent_name: String,
    pub permission_mode: xai_grok_telemetry::enums::PermissionMode,
    pub mcp_server_names: Vec<String>,
    pub lsp_server_names: Vec<String>,
    pub memory_enabled: bool,
    pub auto_update: Option<bool>,
    pub cwd: String,
    /// Filled from the built agent's bridge so `into_event` doesn't re-walk the disk.
    pub skill_names: Vec<String>,
    /// Resolved vendor-compat config, so recorded AGENTS.md names match
    /// what the session actually discovers.
    pub compat: xai_grok_tools::types::compat::CompatConfig,
    pub plugin_registry: Option<std::sync::Arc<xai_grok_agent::plugins::PluginRegistry>>,
    pub plugin_names: Vec<String>,
}

impl SessionHarnessMetrics {
    pub async fn into_event(self, hooks: Vec<HookRegInfo>) -> SessionHarness {
        // One `plugin.loaded` span per enabled plugin at session start.
        if let Some(registry) = self.plugin_registry.as_deref() {
            for plugin in registry.enabled_plugins() {
                tracing::info_span!(
                    "plugin.loaded",
                    plugin_name = %plugin.name,
                    plugin_version = %plugin.version.as_deref().unwrap_or(""),
                    plugin_scope = plugin.scope.id_label(),
                    has_hooks = plugin.has_hooks,
                    has_mcp = plugin.mcp_server_count > 0,
                    skill_count = plugin.skill_count as i64,
                    agent_count = plugin.agent_count as i64,
                    command_path_count = plugin.command_dirs.len() as i64,
                )
                .in_scope(|| {});
            }
        }

        // One `hook.registered` span per configured hook at session start.
        for h in &hooks {
            tracing::info_span!(
                "hook.registered",
                hook_name = %h.name,
                hook_event = %h.event,
                hook_type = %h.hook_type,
                hook_source = %h.source,
            )
            .in_scope(|| {});
        }
        let hook_names: Vec<String> = hooks.into_iter().map(|h| h.name).collect();

        let agents_md_dir_names = xai_grok_agent::prompt::agents_md::read_agents_config_with_paths(
            &self.cwd,
            self.compat,
        )
        .await
        .iter()
        .filter_map(|f| {
            std::path::Path::new(&f.file_path)
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
        })
        .collect();
        SessionHarness {
            session_id: self.session_id,
            client_identifier: self.client_identifier,
            model_id: self.model_id,
            agent_name: self.agent_name,
            permission_mode: self.permission_mode,
            mcp_server_names: self.mcp_server_names,
            plugin_names: self.plugin_names,
            skill_names: self.skill_names,
            lsp_server_names: self.lsp_server_names,
            hook_names,
            agents_md_dir_names,
            memory_enabled: self.memory_enabled,
            // Same signal `SessionNew` carries; recomputed here because this
            // event is built off-thread, after spawn (cheap: repo discovery).
            is_git_repo: xai_grok_telemetry::context::collect_git_context(&self.cwd).is_git_repo,
            auto_update: self.auto_update,
        }
    }
}
