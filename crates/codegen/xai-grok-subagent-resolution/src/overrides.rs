//! Runtime override resolution: merges explicit, role, and persona defaults.
//!
//! Extracted from `xai-grok-shell/src/agent/subagent/` `resolve_effective_overrides()`.

use std::collections::HashMap;
use std::path::Path;

use serde::de::DeserializeOwned;
use xai_grok_tools::implementations::grok_build::task::types::SubagentRuntimeOverrides;
use xai_tool_types::{SubagentCapabilityMode, SubagentIsolationMode, SubagentReasoningEffort};

use crate::config::{SubagentPersona, SubagentRole};
use crate::types::{DefinitionRuntimeDefaults, EffectiveRuntimeConfig};

/// Parse a serde-deserializable enum from a plain string value.
///
/// Used for `SubagentCapabilityMode` and `SubagentIsolationMode` which
/// accept kebab-case string variants via `#[serde(rename_all = "kebab-case")]`.
fn parse_enum_from_str<T: DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_value::<T>(serde_json::Value::String(s.to_string())).ok()
}

pub fn intersect_capability_modes(
    requested: Option<SubagentCapabilityMode>,
    ceiling: Option<SubagentCapabilityMode>,
) -> Option<SubagentCapabilityMode> {
    use SubagentCapabilityMode as Mode;
    match (requested, ceiling) {
        (None, None) => None,
        (Some(mode), None) | (None, Some(mode)) => Some(mode),
        (Some(Mode::All), Some(mode)) | (Some(mode), Some(Mode::All)) => Some(mode),
        (Some(Mode::ReadOnly), Some(_)) | (Some(_), Some(Mode::ReadOnly)) => Some(Mode::ReadOnly),
        (Some(Mode::ReadWrite), Some(Mode::ReadWrite)) => Some(Mode::ReadWrite),
        (Some(Mode::Execute), Some(Mode::Execute)) => Some(Mode::Execute),
        (Some(Mode::ReadWrite), Some(Mode::Execute))
        | (Some(Mode::Execute), Some(Mode::ReadWrite)) => Some(Mode::ReadOnly),
    }
}

/// Resolve effective runtime config from explicit overrides, role defaults,
/// and persona defaults.
///
/// Model, reasoning-effort, and isolation precedence:
/// 1. Explicit spawn-time override (from `SubagentRuntimeOverrides`)
/// 2. Role default (from `SubagentRole` in config)
/// 3. Persona default (looked up by name from the personas map)
/// 4. Agent-definition default
/// 5. None (parent inheritance, handled downstream)
///
/// Capability modes are security ceilings rather than ordinary defaults: the
/// explicit request, role, and agent definition are intersected so a caller can
/// narrow access but cannot widen a read-only definition or role.
///
/// Persona instructions are loaded eagerly: if `instructions_file` is set,
/// the file is read from disk relative to `source_dir` (or `cwd` as fallback).
/// If the file cannot be read, a fatal `persona_error` is set and the function
/// returns early with only the persona name and error populated (all other
/// fields at their defaults). This matches the shell's fail-closed behavior
/// where persona file errors abort resolution before any other fields are wired.
///
/// Role prompt files follow soft degradation: if `prompt_file` cannot be read,
/// a warning is set but the spawn continues without the role prompt.
pub fn resolve_effective_overrides(
    overrides: &SubagentRuntimeOverrides,
    role: Option<&SubagentRole>,
    personas: &HashMap<String, SubagentPersona>,
    cwd: Option<&Path>,
    role_name: Option<String>,
) -> EffectiveRuntimeConfig {
    resolve_subagent_spec(
        overrides,
        role,
        personas,
        cwd,
        role_name,
        None,
        DefinitionRuntimeDefaults::default(),
    )
}

/// Resolve all pure runtime layers at one boundary. Shell-only concerns such as
/// catalog credential materialization, worktree creation, and session spawning
/// remain downstream, but effort/capability/isolation no longer get reopened
/// after this function returns.
pub fn resolve_subagent_spec(
    overrides: &SubagentRuntimeOverrides,
    role: Option<&SubagentRole>,
    personas: &HashMap<String, SubagentPersona>,
    cwd: Option<&Path>,
    role_name: Option<String>,
    // User `[subagents.effort]` pin for this agent type. Sits between the
    // persona layer and the agent-definition default, mirroring how the
    // `[subagents.models]` pin ranks below role/persona model overrides.
    config_effort_pin: Option<SubagentReasoningEffort>,
    definition_defaults: DefinitionRuntimeDefaults,
) -> EffectiveRuntimeConfig {
    // ── Model resolution ─────────────────────────────────────────
    let model_from_override_or_role = overrides
        .model
        .clone()
        .or_else(|| role.and_then(|r| r.model.clone()));

    // ── Reasoning effort resolution ──────────────────────────────
    let reasoning_from_override_or_role = overrides
        .reasoning_effort
        .or_else(|| role.and_then(|r| r.reasoning_effort));

    // ── Capability mode resolution ───────────────────────────────
    let role_capability_mode = role.and_then(|r| {
        r.default_capability_mode
            .as_deref()
            .and_then(parse_enum_from_str::<SubagentCapabilityMode>)
    });
    let capability_mode = intersect_capability_modes(
        intersect_capability_modes(overrides.capability_mode, role_capability_mode),
        definition_defaults.capability_mode,
    );

    // ── Persona resolution ───────────────────────────────────────
    let persona = overrides.persona.clone();
    let resolved_persona = persona.as_deref().and_then(|name| personas.get(name));

    // Persona model/reasoning cascade after role
    let model =
        model_from_override_or_role.or_else(|| resolved_persona.and_then(|p| p.model.clone()));
    let reasoning_effort = reasoning_from_override_or_role
        .or_else(|| resolved_persona.and_then(|p| p.reasoning_effort))
        .or(config_effort_pin)
        .or(definition_defaults.reasoning_effort);

    // ── Persona instructions loading ─────────────────────────────
    // Fail-closed: if persona resolution produces an error (file unreadable,
    // not found, empty), return early with only persona + error populated.
    // All other fields are defaulted. This matches the shell's behavior where
    // persona errors abort spawn before wiring model/isolation.
    let (persona_instructions, persona_error, persona_fatal) =
        resolve_persona_instructions(persona.as_deref(), personas, cwd);
    // File I/O errors are fatal: return early with defaults so the caller
    // can abort the spawn. Config-level errors ("not found", "no instructions")
    // are non-fatal: they set `persona_error` but other fields still resolve.
    // This matches the shell's original behavior where only the file-read
    // error path did `return EffectiveRuntimeConfig { ..Default::default() }`.
    if persona_fatal {
        return EffectiveRuntimeConfig {
            persona,
            persona_error,
            ..Default::default()
        };
    }

    // ── Role prompt file loading (soft degradation) ──────────────
    let mut role_prompt_warning = None;
    let role_prompt = role.and_then(|r| {
        let file_path = r.prompt_file.as_deref()?;
        let base_dir = r.source_dir.as_deref().or(cwd)?;
        match std::fs::read_to_string(base_dir.join(file_path)) {
            Ok(content) => Some(content),
            Err(e) => {
                let msg = format!("role prompt_file \"{file_path}\": {e}");
                tracing::warn!(path = file_path, error = %e, "Failed to read role prompt_file");
                role_prompt_warning = Some(msg);
                None
            }
        }
    });

    // ── Isolation resolution ─────────────────────────────────────
    let isolation = overrides
        .isolation
        .or_else(|| {
            role.and_then(|r| r.default_isolation.as_deref())
                .and_then(parse_enum_from_str::<SubagentIsolationMode>)
        })
        .or_else(|| {
            resolved_persona
                .and_then(|p| p.default_isolation.as_deref())
                .and_then(parse_enum_from_str::<SubagentIsolationMode>)
        })
        .or(definition_defaults.isolation)
        .unwrap_or(SubagentIsolationMode::None);

    EffectiveRuntimeConfig {
        model,
        reasoning_effort,
        capability_mode,
        persona,
        persona_instructions,
        role_prompt,
        role_prompt_warning,
        role_name,
        persona_error,
        isolation,
    }
}

/// Resolve persona instructions from inline text and/or instructions_file.
///
/// Returns `(instructions, error, fatal)`:
/// - `(Some(text), None, false)` on success
/// - `(None, Some(err), true)` for file I/O errors (caller should early-return with defaults)
/// - `(None, Some(err), false)` for config-level errors (persona not found, no instructions)
/// - `(None, None, false)` when no persona is requested
fn resolve_persona_instructions(
    persona_name: Option<&str>,
    personas: &HashMap<String, SubagentPersona>,
    cwd: Option<&Path>,
) -> (Option<String>, Option<String>, bool) {
    let Some(name) = persona_name else {
        return (None, None, false);
    };

    let Some(p) = personas.get(name) else {
        return (
            None,
            Some(format!("persona \"{name}\" not found in config")),
            false, // not fatal — config error, other fields still resolve
        );
    };

    let mut parts = Vec::new();

    if let Some(ref inline) = p.instructions {
        parts.push(inline.clone());
    }

    if let Some(ref file_path) = p.instructions_file {
        let base = p.source_dir.as_deref().or(cwd);
        match base {
            Some(base_dir) => match std::fs::read_to_string(base_dir.join(file_path)) {
                Ok(content) => parts.push(content),
                Err(e) => {
                    let err = format!(
                        "persona \"{name}\": failed to read instructions_file \
                         \"{file_path}\": {e}"
                    );
                    return (None, Some(err), true); // fatal — file I/O error
                }
            },
            None => {
                let err = format!(
                    "persona \"{name}\": cannot resolve instructions_file \
                     \"{file_path}\": no source_dir or cwd available"
                );
                return (None, Some(err), true); // fatal — unresolvable path
            }
        }
    }

    if parts.is_empty() {
        (
            None,
            Some(format!(
                "persona \"{name}\" has no instructions or instructions_file"
            )),
            false, // not fatal — config error, other fields still resolve
        )
    } else {
        (Some(parts.join("\n\n")), None, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_tools::implementations::grok_build::task::types::ModelOverrideProvenance;
    use xai_tool_types::SubagentReasoningEffort;

    /// Helper to build an overrides struct with only the fields we care about.
    fn make_overrides(
        model: Option<&str>,
        persona: Option<&str>,
        capability_mode: Option<SubagentCapabilityMode>,
        isolation: Option<SubagentIsolationMode>,
        reasoning_effort: Option<&str>,
    ) -> SubagentRuntimeOverrides {
        SubagentRuntimeOverrides {
            model: model.map(String::from),
            model_override_provenance: ModelOverrideProvenance::Harness,
            reasoning_effort: reasoning_effort.map(|value| {
                value
                    .parse::<SubagentReasoningEffort>()
                    .expect("valid test reasoning effort")
            }),
            persona: persona.map(String::from),
            capability_mode,
            isolation,
            harness_agent_type: None,
            completion_output_cap: None,
            spawn_depth: None,
            output_token_budget: None,
            output_schema: None,
            loop_task_id: None,
        }
    }

    fn empty_personas() -> HashMap<String, SubagentPersona> {
        HashMap::new()
    }

    // ── Precedence tests ─────────────────────────────────────────

    #[test]
    fn explicit_model_overrides_role() {
        let overrides = make_overrides(Some("grok-light"), None, None, None, None);
        let role = SubagentRole {
            model: Some("grok-3".into()),
            ..Default::default()
        };
        let result =
            resolve_effective_overrides(&overrides, Some(&role), &empty_personas(), None, None);
        assert_eq!(result.model.as_deref(), Some("grok-light"));
    }

    #[test]
    fn role_model_used_when_no_explicit() {
        let overrides = make_overrides(None, None, None, None, None);
        let role = SubagentRole {
            model: Some("grok-3".into()),
            ..Default::default()
        };
        let result =
            resolve_effective_overrides(&overrides, Some(&role), &empty_personas(), None, None);
        assert_eq!(result.model.as_deref(), Some("grok-3"));
    }

    #[test]
    fn persona_model_used_when_no_explicit_or_role() {
        let overrides = make_overrides(None, Some("researcher"), None, None, None);
        let mut personas = HashMap::new();
        personas.insert(
            "researcher".to_string(),
            SubagentPersona {
                model: Some("grok-3-fast".into()),
                instructions: Some("Research things.".into()),
                ..Default::default()
            },
        );
        let result = resolve_effective_overrides(&overrides, None, &personas, None, None);
        assert_eq!(result.model.as_deref(), Some("grok-3-fast"));
    }

    #[test]
    fn no_model_when_none_specified() {
        let overrides = make_overrides(None, None, None, None, None);
        let result = resolve_effective_overrides(&overrides, None, &empty_personas(), None, None);
        assert!(result.model.is_none());
    }

    #[test]
    fn explicit_capability_mode_intersects_role_ceiling() {
        let overrides = make_overrides(None, None, Some(SubagentCapabilityMode::All), None, None);
        let role = SubagentRole {
            default_capability_mode: Some("read-only".into()),
            ..Default::default()
        };
        let result =
            resolve_effective_overrides(&overrides, Some(&role), &empty_personas(), None, None);
        assert_eq!(
            result.capability_mode,
            Some(SubagentCapabilityMode::ReadOnly)
        );
    }

    #[test]
    fn incompatible_write_and_execute_modes_intersect_to_read_only() {
        assert_eq!(
            intersect_capability_modes(
                Some(SubagentCapabilityMode::ReadWrite),
                Some(SubagentCapabilityMode::Execute),
            ),
            Some(SubagentCapabilityMode::ReadOnly)
        );
    }

    #[test]
    fn role_capability_mode_used_when_no_explicit() {
        let overrides = make_overrides(None, None, None, None, None);
        let role = SubagentRole {
            default_capability_mode: Some("read-only".into()),
            ..Default::default()
        };
        let result =
            resolve_effective_overrides(&overrides, Some(&role), &empty_personas(), None, None);
        assert_eq!(
            result.capability_mode,
            Some(SubagentCapabilityMode::ReadOnly)
        );
    }

    #[test]
    fn invalid_role_capability_mode_falls_through() {
        let overrides = make_overrides(None, None, None, None, None);
        let role = SubagentRole {
            default_capability_mode: Some("bogus".into()),
            ..Default::default()
        };
        let result =
            resolve_effective_overrides(&overrides, Some(&role), &empty_personas(), None, None);
        assert!(result.capability_mode.is_none());
    }

    // ── Reasoning effort precedence ──────────────────────────────

    #[test]
    fn explicit_reasoning_effort_overrides_role_and_persona() {
        let overrides = make_overrides(None, Some("p"), None, None, Some("low"));
        let role = SubagentRole {
            reasoning_effort: Some(SubagentReasoningEffort::High),
            ..Default::default()
        };
        let mut personas = HashMap::new();
        personas.insert(
            "p".to_string(),
            SubagentPersona {
                reasoning_effort: Some(SubagentReasoningEffort::Medium),
                instructions: Some("test".into()),
                ..Default::default()
            },
        );
        let result = resolve_effective_overrides(&overrides, Some(&role), &personas, None, None);
        assert_eq!(result.reasoning_effort, Some(SubagentReasoningEffort::Low));
    }

    #[test]
    fn role_reasoning_effort_overrides_persona() {
        let overrides = make_overrides(None, Some("p"), None, None, None);
        let role = SubagentRole {
            reasoning_effort: Some(SubagentReasoningEffort::High),
            ..Default::default()
        };
        let mut personas = HashMap::new();
        personas.insert(
            "p".to_string(),
            SubagentPersona {
                reasoning_effort: Some(SubagentReasoningEffort::Medium),
                instructions: Some("test".into()),
                ..Default::default()
            },
        );
        let result = resolve_effective_overrides(&overrides, Some(&role), &personas, None, None);
        assert_eq!(result.reasoning_effort, Some(SubagentReasoningEffort::High));
    }

    #[test]
    fn persona_reasoning_effort_used_when_no_explicit_or_role() {
        let overrides = make_overrides(None, Some("p"), None, None, None);
        let mut personas = HashMap::new();
        personas.insert(
            "p".to_string(),
            SubagentPersona {
                reasoning_effort: Some(SubagentReasoningEffort::Medium),
                instructions: Some("test".into()),
                ..Default::default()
            },
        );
        let result = resolve_effective_overrides(&overrides, None, &personas, None, None);
        assert_eq!(
            result.reasoning_effort,
            Some(SubagentReasoningEffort::Medium)
        );
    }

    #[test]
    fn definition_defaults_fill_only_unset_runtime_fields() {
        let overrides = make_overrides(None, None, None, None, None);
        let defaults = DefinitionRuntimeDefaults {
            reasoning_effort: Some(SubagentReasoningEffort::High),
            capability_mode: Some(SubagentCapabilityMode::ReadOnly),
            isolation: Some(SubagentIsolationMode::Worktree),
        };

        let result = resolve_subagent_spec(
            &overrides,
            None,
            &empty_personas(),
            None,
            None,
            None,
            defaults,
        );

        assert_eq!(result.reasoning_effort, Some(SubagentReasoningEffort::High));
        assert_eq!(
            result.capability_mode,
            Some(SubagentCapabilityMode::ReadOnly)
        );
        assert_eq!(result.isolation, SubagentIsolationMode::Worktree);
    }

    #[test]
    fn higher_layers_override_defaults_but_cannot_widen_capability_ceiling() {
        let overrides = make_overrides(
            None,
            None,
            Some(SubagentCapabilityMode::All),
            Some(SubagentIsolationMode::None),
            Some("low"),
        );
        let defaults = DefinitionRuntimeDefaults {
            reasoning_effort: Some(SubagentReasoningEffort::High),
            capability_mode: Some(SubagentCapabilityMode::ReadOnly),
            isolation: Some(SubagentIsolationMode::Worktree),
        };

        let result = resolve_subagent_spec(
            &overrides,
            None,
            &empty_personas(),
            None,
            None,
            None,
            defaults,
        );

        assert_eq!(result.reasoning_effort, Some(SubagentReasoningEffort::Low));
        assert_eq!(
            result.capability_mode,
            Some(SubagentCapabilityMode::ReadOnly)
        );
        assert_eq!(result.isolation, SubagentIsolationMode::None);
    }

    // ── [subagents.effort] config pin precedence ─────────────────

    #[test]
    fn config_effort_pin_applies_below_persona_and_above_definition_default() {
        let overrides = make_overrides(None, None, None, None, None);
        let defaults = DefinitionRuntimeDefaults {
            reasoning_effort: Some(SubagentReasoningEffort::Medium),
            ..DefinitionRuntimeDefaults::default()
        };
        // No role/persona layer: the pin wins over the definition default.
        let result = resolve_subagent_spec(
            &overrides,
            None,
            &empty_personas(),
            None,
            None,
            Some(SubagentReasoningEffort::Ultra),
            defaults,
        );
        assert_eq!(
            result.reasoning_effort,
            Some(SubagentReasoningEffort::Ultra)
        );
    }

    #[test]
    fn config_effort_pin_loses_to_persona_and_explicit_override() {
        let mut personas = HashMap::new();
        personas.insert(
            "deep".to_string(),
            SubagentPersona {
                reasoning_effort: Some(SubagentReasoningEffort::Low),
                instructions: Some("test".into()),
                ..Default::default()
            },
        );
        // Persona effort beats the config pin.
        let overrides = make_overrides(None, Some("deep"), None, None, None);
        let result = resolve_subagent_spec(
            &overrides,
            None,
            &personas,
            None,
            None,
            Some(SubagentReasoningEffort::Ultra),
            DefinitionRuntimeDefaults::default(),
        );
        assert_eq!(result.reasoning_effort, Some(SubagentReasoningEffort::Low));

        // Explicit per-spawn override beats the pin too.
        let overrides = make_overrides(None, None, None, None, Some("high"));
        let result = resolve_subagent_spec(
            &overrides,
            None,
            &empty_personas(),
            None,
            None,
            Some(SubagentReasoningEffort::Ultra),
            DefinitionRuntimeDefaults::default(),
        );
        assert_eq!(result.reasoning_effort, Some(SubagentReasoningEffort::High));
    }

    // ── Isolation precedence ─────────────────────────────────────

    #[test]
    fn explicit_isolation_overrides_role() {
        let overrides = make_overrides(
            None,
            None,
            None,
            Some(SubagentIsolationMode::Worktree),
            None,
        );
        let role = SubagentRole {
            default_isolation: Some("none".into()),
            ..Default::default()
        };
        let result =
            resolve_effective_overrides(&overrides, Some(&role), &empty_personas(), None, None);
        assert_eq!(result.isolation, SubagentIsolationMode::Worktree);
    }

    #[test]
    fn role_isolation_used_when_no_explicit() {
        let overrides = make_overrides(None, None, None, None, None);
        let role = SubagentRole {
            default_isolation: Some("worktree".into()),
            ..Default::default()
        };
        let result =
            resolve_effective_overrides(&overrides, Some(&role), &empty_personas(), None, None);
        assert_eq!(result.isolation, SubagentIsolationMode::Worktree);
    }

    #[test]
    fn invalid_role_isolation_falls_through_to_persona() {
        let overrides = make_overrides(None, Some("isolated"), None, None, None);
        let role = SubagentRole {
            default_isolation: Some("invalid".into()),
            ..Default::default()
        };
        let mut personas = HashMap::new();
        personas.insert(
            "isolated".to_string(),
            SubagentPersona {
                instructions: Some("Use isolation.".into()),
                default_isolation: Some("worktree".into()),
                ..Default::default()
            },
        );

        let result = resolve_effective_overrides(&overrides, Some(&role), &personas, None, None);

        assert_eq!(result.isolation, SubagentIsolationMode::Worktree);
    }

    #[test]
    fn isolation_defaults_to_none() {
        let overrides = make_overrides(None, None, None, None, None);
        let result = resolve_effective_overrides(&overrides, None, &empty_personas(), None, None);
        assert_eq!(result.isolation, SubagentIsolationMode::None);
    }

    // ── Persona instruction loading ──────────────────────────────

    #[test]
    fn persona_inline_instructions_only() {
        let overrides = make_overrides(None, Some("writer"), None, None, None);
        let mut personas = HashMap::new();
        personas.insert(
            "writer".to_string(),
            SubagentPersona {
                instructions: Some("Be concise.".into()),
                ..Default::default()
            },
        );
        let result = resolve_effective_overrides(&overrides, None, &personas, None, None);
        assert_eq!(result.persona_instructions.as_deref(), Some("Be concise."));
        assert!(result.persona_error.is_none());
    }

    #[test]
    fn persona_file_instructions_only() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("persona.md");
        std::fs::write(&file_path, "File-based instructions.").unwrap();

        let overrides = make_overrides(None, Some("writer"), None, None, None);
        let mut personas = HashMap::new();
        personas.insert(
            "writer".to_string(),
            SubagentPersona {
                instructions_file: Some("persona.md".into()),
                source_dir: Some(dir.path().to_path_buf()),
                ..Default::default()
            },
        );
        let result = resolve_effective_overrides(&overrides, None, &personas, None, None);
        assert_eq!(
            result.persona_instructions.as_deref(),
            Some("File-based instructions.")
        );
        assert!(result.persona_error.is_none());
    }

    #[test]
    fn persona_inline_and_file_instructions_merged() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("extra.md");
        std::fs::write(&file_path, "From file.").unwrap();

        let overrides = make_overrides(None, Some("writer"), None, None, None);
        let mut personas = HashMap::new();
        personas.insert(
            "writer".to_string(),
            SubagentPersona {
                instructions: Some("From inline.".into()),
                instructions_file: Some("extra.md".into()),
                source_dir: Some(dir.path().to_path_buf()),
                ..Default::default()
            },
        );
        let result = resolve_effective_overrides(&overrides, None, &personas, None, None);
        assert_eq!(
            result.persona_instructions.as_deref(),
            Some("From inline.\n\nFrom file.")
        );
    }

    #[test]
    fn persona_not_found_returns_error() {
        let overrides = make_overrides(None, Some("missing"), None, None, None);
        let result = resolve_effective_overrides(&overrides, None, &empty_personas(), None, None);
        assert!(result.persona_instructions.is_none());
        assert!(result.persona_error.is_some());
        assert!(result.persona_error.as_ref().unwrap().contains("not found"));
    }

    #[test]
    fn persona_empty_instructions_returns_error() {
        let overrides = make_overrides(None, Some("empty"), None, None, None);
        let mut personas = HashMap::new();
        personas.insert("empty".to_string(), SubagentPersona::default());
        let result = resolve_effective_overrides(&overrides, None, &personas, None, None);
        assert!(result.persona_instructions.is_none());
        assert!(result.persona_error.is_some());
        assert!(
            result
                .persona_error
                .as_ref()
                .unwrap()
                .contains("has no instructions")
        );
    }

    #[test]
    fn persona_file_not_found_returns_error() {
        let overrides = make_overrides(None, Some("broken"), None, None, None);
        let dir = tempfile::tempdir().unwrap();
        let mut personas = HashMap::new();
        personas.insert(
            "broken".to_string(),
            SubagentPersona {
                instructions_file: Some("nonexistent.md".into()),
                source_dir: Some(dir.path().to_path_buf()),
                ..Default::default()
            },
        );
        let result = resolve_effective_overrides(&overrides, None, &personas, None, None);
        assert!(result.persona_instructions.is_none());
        assert!(result.persona_error.is_some());
        assert!(
            result
                .persona_error
                .as_ref()
                .unwrap()
                .contains("failed to read")
        );
    }

    // ── Role prompt file loading ─────────────────────────────────

    #[test]
    fn role_prompt_file_loaded_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let prompt_path = dir.path().join("role.md");
        std::fs::write(&prompt_path, "Role instructions here.").unwrap();

        let overrides = make_overrides(None, None, None, None, None);
        let role = SubagentRole {
            prompt_file: Some("role.md".into()),
            source_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let result =
            resolve_effective_overrides(&overrides, Some(&role), &empty_personas(), None, None);
        assert_eq!(
            result.role_prompt.as_deref(),
            Some("Role instructions here.")
        );
        assert!(result.role_prompt_warning.is_none());
    }

    #[test]
    fn role_prompt_file_missing_produces_warning() {
        let dir = tempfile::tempdir().unwrap();
        let overrides = make_overrides(None, None, None, None, None);
        let role = SubagentRole {
            prompt_file: Some("missing.md".into()),
            source_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let result =
            resolve_effective_overrides(&overrides, Some(&role), &empty_personas(), None, None);
        assert!(result.role_prompt.is_none());
        assert!(result.role_prompt_warning.is_some());
    }

    // ── No persona requested ─────────────────────────────────────

    #[test]
    fn no_persona_no_instructions() {
        let overrides = make_overrides(None, None, None, None, None);
        let result = resolve_effective_overrides(&overrides, None, &empty_personas(), None, None);
        assert!(result.persona.is_none());
        assert!(result.persona_instructions.is_none());
        assert!(result.persona_error.is_none());
    }

    // ── Persona with cwd fallback for instructions_file ──────────

    #[test]
    fn persona_instructions_file_uses_cwd_when_no_source_dir() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("instructions.md");
        std::fs::write(&file_path, "CWD-resolved instructions.").unwrap();

        let overrides = make_overrides(None, Some("cwd_persona"), None, None, None);
        let mut personas = HashMap::new();
        personas.insert(
            "cwd_persona".to_string(),
            SubagentPersona {
                instructions_file: Some("instructions.md".into()),
                // source_dir is None - will fall back to cwd
                ..Default::default()
            },
        );
        let result =
            resolve_effective_overrides(&overrides, None, &personas, Some(dir.path()), None);
        assert_eq!(
            result.persona_instructions.as_deref(),
            Some("CWD-resolved instructions.")
        );
        assert!(result.persona_error.is_none());
    }

    // ── Persona error early-return (fail-closed) ─────────────────

    #[test]
    fn persona_not_found_error_is_non_fatal() {
        // "not found" is a config-level error: persona_error is set but
        // other fields still resolve from role/overrides.
        let overrides = make_overrides(Some("grok-3"), Some("missing"), None, None, None);
        let role = SubagentRole {
            model: Some("grok-light".into()),
            default_isolation: Some("worktree".into()),
            ..Default::default()
        };
        let result =
            resolve_effective_overrides(&overrides, Some(&role), &empty_personas(), None, None);
        assert_eq!(
            result.persona_error.as_deref(),
            Some("persona \"missing\" not found in config"),
        );
        // Non-fatal: other fields ARE resolved (explicit model takes precedence)
        assert_eq!(
            result.model.as_deref(),
            Some("grok-3"),
            "explicit model should resolve despite persona error"
        );
    }

    #[test]
    fn persona_file_error_returns_early_with_defaults() {
        // File I/O errors ARE fatal: early return with defaults.
        let overrides = make_overrides(Some("grok-3"), Some("broken"), None, None, None);
        let dir = tempfile::tempdir().unwrap();
        let mut personas = HashMap::new();
        personas.insert(
            "broken".to_string(),
            SubagentPersona {
                instructions_file: Some("nonexistent.md".into()),
                source_dir: Some(dir.path().to_path_buf()),
                ..Default::default()
            },
        );
        let role = SubagentRole {
            model: Some("grok-light".into()),
            ..Default::default()
        };
        let result = resolve_effective_overrides(&overrides, Some(&role), &personas, None, None);
        // Fatal error: persona_error is set
        assert!(
            result
                .persona_error
                .as_deref()
                .unwrap()
                .contains("failed to read")
        );
        // All other fields are at Default (early return)
        assert!(
            result.model.is_none(),
            "model should be None on fatal persona error"
        );
    }

    // ── instructions_file with no base dir ────────────────────────

    #[test]
    fn persona_instructions_file_no_base_dir_returns_error() {
        let overrides = make_overrides(None, Some("orphan"), None, None, None);
        let mut personas = HashMap::new();
        personas.insert(
            "orphan".to_string(),
            SubagentPersona {
                instructions_file: Some("orphan.md".into()),
                // source_dir is None AND cwd will be None
                ..Default::default()
            },
        );
        let result = resolve_effective_overrides(&overrides, None, &personas, None, None);
        assert!(result.persona_error.is_some());
        assert_eq!(
            result.persona_error.as_deref(),
            Some(
                "persona \"orphan\": cannot resolve instructions_file \
                 \"orphan.md\": no source_dir or cwd available"
            ),
        );
    }

    // ── Persona isolation fallback (role has no isolation, persona does) ──

    #[test]
    fn persona_isolation_used_when_no_explicit_or_role() {
        let overrides = make_overrides(None, Some("p"), None, None, None);
        let mut personas = HashMap::new();
        personas.insert(
            "p".to_string(),
            SubagentPersona {
                instructions: Some("test".into()),
                default_isolation: Some("worktree".into()),
                ..Default::default()
            },
        );
        // No role, no explicit isolation — should fall through to persona
        let result = resolve_effective_overrides(&overrides, None, &personas, None, None);
        assert_eq!(result.isolation, SubagentIsolationMode::Worktree);
    }

    // ── Role prompt file cwd fallback ─────────────────────────────

    #[test]
    fn role_prompt_file_uses_cwd_when_no_source_dir() {
        let dir = tempfile::tempdir().unwrap();
        let prompt_path = dir.path().join("role.md");
        std::fs::write(&prompt_path, "CWD role instructions.").unwrap();

        let overrides = make_overrides(None, None, None, None, None);
        let role = SubagentRole {
            prompt_file: Some("role.md".into()),
            // source_dir is None — falls back to cwd
            ..Default::default()
        };
        let result = resolve_effective_overrides(
            &overrides,
            Some(&role),
            &empty_personas(),
            Some(dir.path()),
            None,
        );
        assert_eq!(
            result.role_prompt.as_deref(),
            Some("CWD role instructions."),
        );
        assert!(result.role_prompt_warning.is_none());
    }

    // ── role_name parameter is threaded through ───────────────────

    #[test]
    fn role_name_parameter_threaded_through() {
        let overrides = make_overrides(None, None, None, None, None);
        let result = resolve_effective_overrides(
            &overrides,
            None,
            &empty_personas(),
            None,
            Some("my-role".into()),
        );
        assert_eq!(result.role_name.as_deref(), Some("my-role"));
    }
}
