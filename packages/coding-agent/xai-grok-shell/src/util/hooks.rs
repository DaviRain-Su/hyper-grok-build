use std::path::{Path, PathBuf};

use xai_grok_config::resolve_global_hook_sources;
use xai_grok_hooks::discovery::HookSource;
use xai_grok_hooks::error::HookError;

/// Owned paths for hook sources. Callers borrow via `as_sources()`.
pub(crate) struct HookSourcePaths {
    pub global: Vec<PathBuf>,
    pub project: Vec<PathBuf>,
}

impl HookSourcePaths {
    /// Borrow as `HookSource` refs. Project sources are excluded when untrusted.
    pub(crate) fn as_sources(
        &self,
        include_project: bool,
    ) -> (Vec<HookSource<'_>>, Vec<HookSource<'_>>) {
        let global = self.global.iter().map(|p| path_to_source(p)).collect();
        let project = if include_project {
            self.project.iter().map(|p| path_to_source(p)).collect()
        } else {
            vec![]
        };
        (global, project)
    }
}

fn path_to_source(p: &Path) -> HookSource<'_> {
    if p.is_dir() {
        HookSource::Directory(p)
    } else {
        HookSource::SettingsFile(p)
    }
}

fn include_claude_hooks(compat: &xai_grok_tools::types::compat::CompatConfig) -> bool {
    compat.claude.hooks
        && !crate::claude_import::is_claude_import_marked_with_log("discover_hook_source_paths")
}

fn include_cursor_hooks(compat: &xai_grok_tools::types::compat::CompatConfig) -> bool {
    compat.cursor.hooks
}

/// Global and project hook source paths.
/// The registry file is never a discovery source; Claude and Cursor sources are appended when their compat gates are on.
pub(crate) fn discover_hook_source_paths(
    git_root: Option<&Path>,
    compat: &xai_grok_tools::types::compat::CompatConfig,
) -> HookSourcePaths {
    let grok = xai_grok_config::user_grok_home();
    let home = xai_dirs::home_dir();
    let include_claude = include_claude_hooks(compat);
    let include_cursor = include_cursor_hooks(compat);

    // An unreadable hooks-paths file keeps the fixed Grok sources; a hard resolve failure omits all Grok global sources
    let mut global: Vec<PathBuf> =
        match resolve_global_hook_sources(grok.as_deref(), /* reject_symlinks */ false) {
            Ok(resolved) => {
                if let Some(e) = &resolved.configured_error {
                    tracing::warn!(
                        error = %e,
                        "hooks-paths unreadable; retaining fixed Grok hook discovery sources only"
                    );
                }
                resolved
                    .discovery_sources()
                    .map(|s| s.path.clone())
                    .collect()
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "global hook source resolve hard-failed; omitting Grok global sources"
                );
                Vec::new()
            }
        };

    if let Some(h) = home.as_deref() {
        if include_claude {
            global.push(h.join(".claude").join("settings.json"));
            global.push(h.join(".claude").join("settings.local.json"));
        }
        if include_cursor {
            global.push(h.join(".cursor").join("hooks.json"));
        }
    }

    let mut project = Vec::new();
    if let Some(root) = git_root {
        if include_claude {
            project.push(root.join(".claude").join("settings.json"));
            project.push(root.join(".claude").join("settings.local.json"));
        }
        project.push(root.join(".grok").join("hooks"));
        if include_cursor {
            project.push(root.join(".cursor").join("hooks.json"));
        }
    }

    HookSourcePaths { global, project }
}

/// Single load entry point: build compat-aware sources, gate project sources on trust, then load.
/// Every session-startup and mid-session reload site routes through here so the source policy stays in one place.
pub(crate) fn discover_hooks(
    git_root: Option<&Path>,
    compat: &xai_grok_tools::types::compat::CompatConfig,
    trusted: bool,
) -> (xai_grok_hooks::discovery::HookRegistry, Vec<HookError>) {
    // Read fresh each call (not cached): a mid-session `/hooks` reload must see an updated `config.toml` or `managed_config.toml`
    // This is lighter than `ConfigLayers::load` (only the small per-layer files, no campaigns, version overrides, or MDM)
    let config_layers = xai_grok_config::hook_config_layers();
    assemble_hooks(&config_layers, git_root, compat, trusted)
}

/// Pure, injectable core: combine config-layer hooks with file-source hooks and dedup once.
/// Config-layer specs go first.
/// The first-wins dedup in [`xai_grok_hooks::discovery::registry_from_specs_deduped`] then lets a config hook beat a byte-identical file hook.
/// `config_layers` is a parameter (not read here) so tests can drive it with hand-built layers.
pub(crate) fn assemble_hooks(
    config_layers: &[xai_grok_config::HookConfigLayer],
    git_root: Option<&Path>,
    compat: &xai_grok_tools::types::compat::CompatConfig,
    trusted: bool,
) -> (xai_grok_hooks::discovery::HookRegistry, Vec<HookError>) {
    let (mut specs, mut errors) =
        xai_grok_hooks::config::parse_hooks_from_config_layers(config_layers);

    let source_paths = discover_hook_source_paths(git_root, compat);
    let (global_sources, project_sources) = source_paths.as_sources(trusted);
    let (file_specs, file_errors) =
        xai_grok_hooks::discovery::collect_specs_from_sources(&global_sources, &project_sources);
    specs.extend(file_specs);
    errors.extend(file_errors);

    (
        xai_grok_hooks::discovery::registry_from_specs_deduped(specs),
        errors,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_hooks::config::HookProvenance;
    use xai_grok_hooks::event::HookEventName;

    fn write_requirements(dir: &Path, content: &str) {
        std::fs::write(dir.join("requirements.toml"), content).unwrap();
    }

    /// A temp policy layer pins hooks for `SessionStart`, `UserPromptSubmit`, and `PreToolUse`.
    /// It flows through the real requirements read (`hook_config_layers_at`) and the real assembly (`assemble_hooks`).
    /// All three register with `Requirements` provenance, the provenance the disable exemption keys on.
    #[test]
    fn requirements_layer_pins_hooks_with_requirements_provenance() {
        let system_dir = tempfile::tempdir().unwrap();
        write_requirements(
            system_dir.path(),
            r#"
[[hooks.SessionStart]]
[[hooks.SessionStart.hooks]]
type = "command"
command = "/opt/policy/pin-session-start.sh"
timeout = 5

[[hooks.UserPromptSubmit]]
[[hooks.UserPromptSubmit.hooks]]
type = "command"
command = "/opt/policy/pin-prompt-submit.sh"
timeout = 5

[[hooks.PreToolUse]]
matcher = "*"
[[hooks.PreToolUse.hooks]]
type = "command"
command = "/opt/policy/pin-pre-tool-use.sh"
timeout = 5
"#,
        );

        let layers = xai_grok_config::hook_config_layers_at(Some(system_dir.path()), None);
        assert_eq!(layers.len(), 1, "one requirements layer expected");
        assert_eq!(layers[0].provenance(), HookProvenance::Requirements);
        assert_eq!(layers[0].source_name(), "requirements/system");

        let compat = xai_grok_tools::types::compat::CompatConfig::default();
        let (registry, errors) = assemble_hooks(&layers, None, &compat, false);
        assert!(errors.is_empty(), "errors: {errors:?}");

        for (event, command) in [
            (HookEventName::SessionStart, "pin-session-start.sh"),
            (HookEventName::UserPromptSubmit, "pin-prompt-submit.sh"),
            (HookEventName::PreToolUse, "pin-pre-tool-use.sh"),
        ] {
            let spec = registry
                .hooks_for(event)
                .iter()
                .find(|s| {
                    s.command_raw
                        .as_deref()
                        .is_some_and(|c| c.contains(command))
                })
                .unwrap_or_else(|| panic!("pinned {event} hook must register"));
            assert_eq!(
                spec.layer,
                HookProvenance::Requirements,
                "pinned {event} hook must carry requirements provenance"
            );
            assert!(
                spec.is_managed_policy(),
                "requirements provenance must classify as managed policy"
            );
            assert!(
                spec.name.starts_with("requirements/system:"),
                "provenance-prefixed name expected, got {}",
                spec.name
            );
        }
    }

    /// A realistic enterprise policy hooks shape parses and registers through the real path.
    /// The shape: command hooks with `timeout: 5`, `PreToolUse` with `matcher: "*"` and two hooks in one group, and matcher-less lifecycle groups.
    /// The two `PreToolUse` hooks are byte-identical, so both parse but content dedup registers one effective hook.
    #[test]
    fn enterprise_policy_hooks_shape_registers() {
        let system_dir = tempfile::tempdir().unwrap();
        write_requirements(
            system_dir.path(),
            r#"
[[hooks.SessionStart]]
[[hooks.SessionStart.hooks]]
type = "command"
command = "policy/hooks/bin/lifecycle-audit.sh"
timeout = 5

[[hooks.PreToolUse]]
matcher = "*"
[[hooks.PreToolUse.hooks]]
type = "command"
command = "policy/hooks/bin/pretooluse-audit.sh"
timeout = 5
[[hooks.PreToolUse.hooks]]
type = "command"
command = "policy/hooks/bin/pretooluse-audit.sh"
timeout = 5

[[hooks.UserPromptSubmit]]
[[hooks.UserPromptSubmit.hooks]]
type = "command"
command = "policy/hooks/bin/lifecycle-audit.sh"
timeout = 5
"#,
        );

        let layers = xai_grok_config::hook_config_layers_at(Some(system_dir.path()), None);
        assert_eq!(layers.len(), 1);

        // Parse level: the verbatim structure yields both PreToolUse handlers.
        let (specs, errors) = xai_grok_hooks::config::parse_hooks_from_config_layers(&layers);
        assert!(errors.is_empty(), "errors: {errors:?}");
        let pre_specs: Vec<_> = specs
            .iter()
            .filter(|s| s.event == HookEventName::PreToolUse)
            .collect();
        assert_eq!(
            pre_specs.len(),
            2,
            "the PreToolUse group's two hooks must both parse"
        );
        for spec in &pre_specs {
            assert_eq!(spec.configured_matcher.as_deref(), Some("*"));
            let matcher = spec.matcher.as_ref().expect("matcher '*' compiles");
            assert!(
                matcher.is_match("run_terminal_command") && matcher.is_match("Bash"),
                "matcher '*' must match every tool"
            );
            assert_eq!(spec.timeout_ms, 5000, "timeout 5s converts to 5000ms");
        }

        // Registry level through the real assembly: all three events register with requirements provenance
        // The byte-identical PreToolUse duplicate collapses to one effective hook
        let compat = xai_grok_tools::types::compat::CompatConfig::default();
        let (registry, errors) = assemble_hooks(&layers, None, &compat, false);
        assert!(errors.is_empty(), "errors: {errors:?}");
        for event in [
            HookEventName::SessionStart,
            HookEventName::UserPromptSubmit,
            HookEventName::PreToolUse,
        ] {
            let policy_hooks: Vec<_> = registry
                .hooks_for(event)
                .iter()
                .filter(|s| s.layer == HookProvenance::Requirements)
                .collect();
            assert!(
                !policy_hooks.is_empty(),
                "pinned {event} hook must register with requirements provenance"
            );
        }
        assert_eq!(
            registry
                .hooks_for(HookEventName::PreToolUse)
                .iter()
                .filter(|s| s.layer == HookProvenance::Requirements)
                .count(),
            1,
            "byte-identical duplicate collapses under content dedup"
        );
    }
}
