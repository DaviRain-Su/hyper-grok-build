//! ACP slash command advertising and resolution.

use std::collections::{HashMap, HashSet};

use agent_client_protocol as acp;
use xai_grok_tools::implementations::skills::skill::format_skill_name;
use xai_grok_tools::implementations::skills::types::SkillInfo;

/// A built-in slash command.
pub(crate) struct BuiltinCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub argument_hint: Option<&'static str>,
    pub aliases: &'static [&'static str],
    /// Capability the agent must have for this command to be useful.
    /// Filtered by `CommandAvailability::allows()` at advertising time;
    /// commands that map to `BuiltinGate::AlwaysOn` are never gated.
    pub gate: BuiltinGate,
    resolve: fn(args: &str) -> BuiltinAction,
}

/// Capability gate that decides whether a `BuiltinCommand` is advertised
/// and resolvable in a given session.
///
/// Each variant maps to a feature/tool the agent must actually have:
/// - `Memory`: a memory backend is configured (`SessionMemory::is_enabled`).
/// - `Scheduler`: `scheduler_create` is registered.
/// - `Hooks`: a hook registry is loaded.
/// - `Plugins`: a plugin registry is loaded.
/// - `Feedback`: the feedback manager is enabled.
/// - `MemoryConfigured`: memory backend params exist (may be currently
///   disabled). Used for `/memory` so the user can re-enable via toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuiltinGate {
    AlwaysOn,
    Feedback,
    Memory,
    MemoryConfigured,
    /// Checks `scheduler_create` only. If any future shell-side builtin
    /// needs a separate scheduler-delete gate, add a `SchedulerDelete` variant.
    Scheduler,
    Hooks,
    Plugins,
    Goal,
    WorkflowLaunches,
    WorkflowManagement,
}

/// All built-in slash commands. Order here = display order in autocomplete.
pub(super) const BUILTIN_COMMANDS: &[BuiltinCommand] = &[
    BuiltinCommand {
        name: "compact",
        description: "Compress conversation history to save context window",
        argument_hint: Some("optional context about what to preserve"),
        aliases: &[],
        gate: BuiltinGate::AlwaysOn,
        resolve: |args| BuiltinAction::Compact {
            user_context: if args.is_empty() {
                None
            } else {
                Some(args.to_string())
            },
        },
    },
    BuiltinCommand {
        name: "always-approve",
        description: "Toggle always-approve mode (skip all permission prompts)",
        argument_hint: Some("on|off"),
        aliases: &["yolo"],
        gate: BuiltinGate::AlwaysOn,
        resolve: |args| BuiltinAction::SetYolo {
            enabled: !matches!(
                args.to_lowercase().as_str(),
                "off" | "false" | "0" | "no" | "disable"
            ),
        },
    },
    BuiltinCommand {
        name: "flush",
        description: "Flush conversation memory to disk now",
        argument_hint: None,
        aliases: &[],
        gate: BuiltinGate::Memory,
        resolve: |_args| BuiltinAction::FlushMemory,
    },
    BuiltinCommand {
        name: "dream",
        description: "Run memory consolidation (merge session logs into organized topics)",
        argument_hint: None,
        aliases: &[],
        gate: BuiltinGate::Memory,
        resolve: |_args| BuiltinAction::Dream,
    },
    BuiltinCommand {
        name: "memory",
        description: "Browse, view, and manage your memories",
        argument_hint: Some("on|off"),
        aliases: &["mem"],
        gate: BuiltinGate::MemoryConfigured,
        resolve: |args| {
            let trimmed = args.trim().to_lowercase();
            match trimmed.as_str() {
                "on" | "enable" => BuiltinAction::MemoryToggle { enabled: true },
                "off" | "disable" => BuiltinAction::MemoryToggle { enabled: false },
                _ => BuiltinAction::MemoryBrowse,
            }
        },
    },
    BuiltinCommand {
        name: "context",
        description: "Show context window usage and session stats",
        argument_hint: None,
        aliases: &[],
        gate: BuiltinGate::AlwaysOn,
        resolve: |_args| BuiltinAction::ContextInfo,
    },
    BuiltinCommand {
        name: "hooks-trust",
        description: "Trust this project for hook execution",
        argument_hint: None,
        aliases: &[],
        gate: BuiltinGate::Hooks,
        resolve: |_args| BuiltinAction::HooksTrust,
    },
    BuiltinCommand {
        name: "hooks-list",
        description: "Show hooks loaded in this session",
        argument_hint: None,
        aliases: &[],
        gate: BuiltinGate::Hooks,
        resolve: |_args| BuiltinAction::HooksList,
    },
    BuiltinCommand {
        name: "hooks-add",
        description: "Add a custom hook file or directory",
        argument_hint: Some("path to hook file or directory"),
        aliases: &[],
        gate: BuiltinGate::Hooks,
        resolve: |args| BuiltinAction::HooksAdd {
            path: args.trim().to_string(),
        },
    },
    BuiltinCommand {
        name: "hooks-remove",
        description: "Remove a custom hook file or directory path",
        argument_hint: Some("path to hook file or directory"),
        aliases: &[],
        gate: BuiltinGate::Hooks,
        resolve: |args| BuiltinAction::HooksRemove {
            path: args.trim().to_string(),
        },
    },
    BuiltinCommand {
        name: "hooks-untrust",
        description: "Remove trust for the current project",
        argument_hint: None,
        aliases: &[],
        gate: BuiltinGate::Hooks,
        resolve: |_args| BuiltinAction::HooksUntrust,
    },
    BuiltinCommand {
        name: "plugins",
        description: "Manage plugins (list, reload, trust, add, remove)",
        argument_hint: Some("list | reload | trust <path> | add <path> | remove <path>"),
        aliases: &["plugin"],
        gate: BuiltinGate::Plugins,
        resolve: |args| {
            let trimmed = args.trim();
            if trimmed.is_empty() || trimmed == "list" {
                BuiltinAction::PluginsList
            } else if trimmed == "reload" {
                BuiltinAction::PluginsReload
            } else if trimmed.starts_with("trust") {
                BuiltinAction::PluginsTrust
            } else if let Some(path) = trimmed.strip_prefix("add ") {
                BuiltinAction::PluginsAdd {
                    path: path.trim().to_string(),
                }
            } else if let Some(path) = trimmed.strip_prefix("remove ") {
                BuiltinAction::PluginsRemove {
                    path: path.trim().to_string(),
                }
            } else if let Some(args) = trimmed.strip_prefix("install ") {
                let args = args.trim();
                let trust = args.ends_with(" --trust") || args == "--trust";
                let source = if trust {
                    args.trim_end_matches(" --trust").trim().to_string()
                } else {
                    args.to_string()
                };
                BuiltinAction::PluginsInstall { source, trust }
            } else if let Some(args) = trimmed.strip_prefix("uninstall ") {
                let args = args.trim();
                let confirm = args.ends_with(" --confirm") || args == "--confirm";
                let name = if confirm {
                    args.trim_end_matches(" --confirm").trim().to_string()
                } else {
                    args.to_string()
                };
                BuiltinAction::PluginsUninstall { name, confirm }
            } else if trimmed == "update" {
                BuiltinAction::PluginsUpdate { name: None }
            } else if let Some(name) = trimmed.strip_prefix("update ") {
                BuiltinAction::PluginsUpdate {
                    name: Some(name.trim().to_string()),
                }
            } else {
                BuiltinAction::PluginsList
            }
        },
    },
    BuiltinCommand {
        name: "reload-plugins",
        description: "Reload plugins from disk (alias for /plugins reload)",
        argument_hint: None,
        aliases: &[],
        gate: BuiltinGate::Plugins,
        resolve: |_args| BuiltinAction::PluginsReload,
    },
    BuiltinCommand {
        name: "session-info",
        description: "Show session details (model, turns, context usage)",
        argument_hint: None,
        aliases: &["status", "info"],
        gate: BuiltinGate::AlwaysOn,
        resolve: |_args| BuiltinAction::SessionInfo,
    },
    BuiltinCommand {
        name: "feedback",
        description: "Send feedback about the current session",
        argument_hint: Some("feedback text"),
        aliases: &[],
        gate: BuiltinGate::Feedback,
        resolve: |args| BuiltinAction::Feedback {
            text: args.trim().to_string(),
        },
    },
    BuiltinCommand {
        name: "deep-research",
        description: "Research with bounded parallel agents, cross-check evidence, and write a cited report",
        argument_hint: Some("<query>"),
        aliases: &[],
        gate: BuiltinGate::WorkflowLaunches,
        resolve: |args| BuiltinAction::DeepResearch {
            query: args.trim().to_string(),
        },
    },
    BuiltinCommand {
        name: "workflow",
        description: "Launch a saved workflow, or manage a run (pause, resume, stop, save)",
        argument_hint: Some("<name> [args] | pause|resume|stop|save [name]"),
        aliases: &[],
        gate: BuiltinGate::WorkflowManagement,
        resolve: |args| {
            const OPS: [&str; 4] = ["pause", "resume", "stop", "save"];
            let trimmed = args.trim();
            let mut parts = trimmed.split_whitespace();
            let first = parts.next().unwrap_or_default();
            let second = parts.next().unwrap_or_default();
            let first_is_op = OPS.contains(&first.to_lowercase().as_str());
            let second_is_final_op =
                OPS.contains(&second.to_lowercase().as_str()) && parts.next().is_none();
            if first.is_empty() || first_is_op || second_is_final_op {
                let (op, run_id) = if first_is_op {
                    (
                        first.to_lowercase(),
                        trimmed[first.len()..].trim_start().to_string(),
                    )
                } else if second_is_final_op {
                    (second.to_lowercase(), first.to_string())
                } else {
                    (String::new(), String::new())
                };
                BuiltinAction::WorkflowManage { run_id, op }
            } else {
                BuiltinAction::WorkflowLaunch {
                    name: first.to_string(),
                    input: trimmed[first.len()..].trim_start().to_string(),
                }
            }
        },
    },
    BuiltinCommand {
        name: "goal",
        description: "Set, manage, or check an autonomous goal",
        argument_hint: Some("<objective> [--budget <tokens>] | status | pause | resume | clear"),
        aliases: &[],
        gate: BuiltinGate::Goal,
        resolve: |args| {
            let trimmed = args.trim();
            match trimmed.to_lowercase().as_str() {
                "" | "status" => BuiltinAction::GoalStatus,
                "pause" => BuiltinAction::GoalPause,
                "resume" => BuiltinAction::GoalResume,
                "clear" => BuiltinAction::GoalClear,
                _ => {
                    let (objective, token_budget) = parse_goal_budget(trimmed);
                    BuiltinAction::GoalSet {
                        objective,
                        token_budget,
                    }
                }
            }
        },
    },
];

/// Split a trailing `--budget <tokens>` flag off a `/goal` objective.
///
/// Only a TRAILING, standalone flag is consumed: the flag must be its own
/// whitespace-separated token and the value a final all-digit positive
/// token. Anything else stays part of the objective so a goal text that
/// merely mentions the flag is never silently mangled.
fn parse_goal_budget(trimmed: &str) -> (String, Option<i64>) {
    if let Some((head, tail)) = trimmed.rsplit_once("--budget") {
        let value = tail.trim();
        let flag_is_own_token = head.ends_with(char::is_whitespace)
            && tail.starts_with(char::is_whitespace)
            && !value.contains(char::is_whitespace);
        let head = head.trim_end();
        if flag_is_own_token
            && !head.is_empty()
            && !value.is_empty()
            && value.bytes().all(|b| b.is_ascii_digit())
            && let Ok(budget) = value.parse::<i64>()
            && budget > 0
        {
            return (head.to_string(), Some(budget));
        }
    }
    (trimmed.to_string(), None)
}

const PROMPT_COMMANDS: &[BuiltinCommand] = &[BuiltinCommand {
    name: "loop",
    description: "Run a prompt on a recurring interval",
    argument_hint: Some("[interval] <prompt>"),
    aliases: &[],
    gate: BuiltinGate::Scheduler,
    // INVARIANT: resolve() short-circuits any prompt-only command via a
    // PROMPT_COMMANDS lookup before reaching this closure. If a future
    // refactor changes that ordering this `unreachable!` will surface
    // the bug loudly instead of silently dispatching to ContextInfo
    // (which is what the previous sentinel did).
    resolve: |_| unreachable!("/loop is dispatched via the PROMPT_COMMANDS path in resolve()"),
}];

/// Per-session capability snapshot used to gate which built-in slash
/// commands the shell advertises and resolves.
///
/// Each field corresponds to a `BuiltinGate` variant. Construct via
/// `CommandAvailability::all_enabled()` for tests, or build it from a
/// live `SessionActor` (see the call site in `acp_session.rs`).
///
/// `Default` returns every gate disabled (fail-closed) so a forgotten
/// initialization advertises only `BuiltinGate::AlwaysOn` commands.
/// In test code, prefer `all_enabled()` when the gating itself isn't
/// under test -- otherwise the test will silently lose coverage of any
/// gated builtin.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CommandAvailability {
    pub feedback: bool,
    /// Memory backend is enabled AND the active toolset includes the
    /// memory read tools. `/flush` and `/dream` only make sense when the
    /// model can later read back what they wrote, so the read-side tool
    /// presence is the right signal -- harnesses that don't register
    /// `memory_search`/`memory_get` get the commands hidden without the
    /// gating layer needing to know about agent_type.
    pub memory: bool,
    /// Memory backend is configured (has `backend_params`) but not
    /// necessarily currently enabled. Gates `/memory` (browse + toggle)
    /// so the user can re-enable memory after toggling it off.
    pub memory_configured: bool,
    pub scheduler: bool,
    pub hooks: bool,
    pub plugins: bool,
    pub goal: bool,
    pub workflows: bool,
    pub workflow_management: bool,
}

impl CommandAvailability {
    /// `true` if commands gated on `gate` should be advertised this session.
    pub fn allows(&self, gate: BuiltinGate) -> bool {
        match gate {
            BuiltinGate::AlwaysOn => true,
            BuiltinGate::Feedback => self.feedback,
            BuiltinGate::Memory => self.memory,
            BuiltinGate::MemoryConfigured => self.memory_configured,
            BuiltinGate::Scheduler => self.scheduler,
            BuiltinGate::Hooks => self.hooks,
            BuiltinGate::Plugins => self.plugins,
            BuiltinGate::Goal => self.goal,
            BuiltinGate::WorkflowLaunches => self.workflows,
            BuiltinGate::WorkflowManagement => self.workflows || self.workflow_management,
        }
    }

    /// Test helper: every gate satisfied (matches the legacy "feedback only"
    /// fixture but enables every newly-gated command too).
    #[cfg(test)]
    pub fn all_enabled() -> Self {
        Self {
            feedback: true,
            memory: true,
            memory_configured: true,
            scheduler: true,
            hooks: true,
            plugins: true,
            goal: true,
            workflows: true,
            workflow_management: true,
        }
    }
}

/// Build the JSON value for `AvailableCommandsUpdate.meta` containing the
/// agent's currently-registered tool names.
///
/// Wire format: `{"tools": ["read_file", "scheduler_create", ...]}`.
/// Pager clients drain this and call `CommandRegistry::set_available_tools`
/// to gate tool-dependent commands like `/loop`.
///
/// Takes `&[String]` rather than `&[&str]` because serde_json copies
/// each entry into the `Value` regardless, so an intermediate
/// `Vec<&str>` adapter would just waste an allocation.
pub(crate) fn build_tools_meta(tool_names: &[String]) -> acp::Meta {
    let mut meta = acp::Meta::new();
    meta.insert("tools".to_owned(), serde_json::json!(tool_names));
    meta
}

struct EffectiveCommandCatalog<'a> {
    builtins: Vec<&'a BuiltinCommand>,
    skills: Vec<SkillCommand<'a>>,
    workflows: Vec<&'a crate::session::workflow::registry::WorkflowListing>,
}

struct SkillCommand<'a> {
    name: String,
    skill: &'a SkillInfo,
}

impl<'a> EffectiveCommandCatalog<'a> {
    fn build(
        skills: &'a [SkillInfo],
        availability: CommandAvailability,
        workflows: &'a [crate::session::workflow::registry::WorkflowListing],
    ) -> Self {
        let builtins: Vec<_> = BUILTIN_COMMANDS
            .iter()
            .chain(PROMPT_COMMANDS.iter())
            .filter(|builtin| availability.allows(builtin.gate))
            .collect();
        const PAGER_COMMAND_KEYS: &[&str] = &[
            "agents",
            "agents-dashboard",
            "always-approve",
            "announcements",
            "auto",
            "btw",
            "cd",
            "changelog",
            "chat",
            "clear",
            "cloud",
            "compact",
            "compact-mode",
            "config",
            "config-agents",
            "context",
            "copy",
            "cost",
            "dashboard",
            "debug",
            "docs",
            "doctor",
            "effort",
            "exit",
            "expand",
            "export",
            "feedback",
            "find",
            "fork",
            "full",
            "fullscreen",
            "gboom",
            "guides",
            "help",
            "history",
            "home",
            "hooks",
            "howto",
            "imagine",
            "imagine-video",
            "import-claude",
            "jump",
            "login",
            "logout",
            "log",
            "loop",
            "m",
            "marketplace",
            "mcps",
            "minimal",
            "ml",
            "model",
            "multiline",
            "new",
            "personas",
            "plan",
            "plan-view",
            "plugins",
            "preferences",
            "prefs",
            "privacy",
            "queue",
            "quit",
            "recap",
            "release-notes",
            "remember",
            "rename",
            "resume",
            "rewind",
            "scroll-debug",
            "session-info",
            "sessions",
            "settings",
            "share",
            "show-plan",
            "skills",
            "tasks",
            "terminal-check",
            "terminal-info",
            "terminal-setup",
            "theme",
            "timeline",
            "timestamps",
            "title",
            "toggle-mouse-reporting",
            "transcript",
            "t",
            "usage",
            "view-plan",
            "vim-mode",
            "voice",
            "welcome",
            "workflows",
            "yolo",
        ];
        let mut taken: HashSet<String> = builtins
            .iter()
            .flat_map(|builtin| {
                std::iter::once(builtin.name)
                    .chain(builtin.aliases.iter().copied())
                    .map(str::to_owned)
            })
            .chain(PAGER_COMMAND_KEYS.iter().copied().map(str::to_owned))
            .collect();

        let candidates: Vec<_> = skills
            .iter()
            .filter(|skill| skill.user_invocable && skill.enabled)
            .collect();
        let mut bare_counts: HashMap<&str, usize> = HashMap::new();
        let mut qualified_counts: HashMap<String, usize> = HashMap::new();
        for skill in &candidates {
            *bare_counts.entry(skill.name.as_str()).or_default() += 1;
            *qualified_counts
                .entry(format_skill_name(skill))
                .or_default() += 1;
        }
        let mut effective_skills = Vec::new();
        for skill in candidates {
            let bare_available = bare_counts.get(skill.name.as_str()) == Some(&1)
                && !taken.contains(skill.name.as_str());
            let name = if bare_available {
                skill.name.clone()
            } else {
                let qualified = format_skill_name(skill);
                if qualified_counts.get(&qualified) != Some(&1) || taken.contains(&qualified) {
                    continue;
                }
                qualified
            };
            taken.insert(name.clone());
            effective_skills.push(SkillCommand { name, skill });
        }
        taken.extend(bare_counts.keys().map(|name| (*name).to_owned()));

        let effective_workflows = if availability.allows(BuiltinGate::WorkflowLaunches) {
            let mut counts: HashMap<&str, usize> = HashMap::new();
            for workflow in workflows {
                *counts.entry(workflow.name.as_str()).or_default() += 1;
            }
            workflows
                .iter()
                .filter(|workflow| {
                    counts.get(workflow.name.as_str()) == Some(&1)
                        && !taken.contains(workflow.name.as_str())
                })
                .collect()
        } else {
            Vec::new()
        };
        Self {
            builtins,
            skills: effective_skills,
            workflows: effective_workflows,
        }
    }

    fn skill(&self, name: &str) -> Option<&'a SkillInfo> {
        self.skills
            .iter()
            .find(|command| command.name == name)
            .map(|command| command.skill)
    }

    fn workflow(
        &self,
        name: &str,
    ) -> Option<&'a crate::session::workflow::registry::WorkflowListing> {
        self.workflows
            .iter()
            .copied()
            .find(|workflow| workflow.name == name)
    }
}

/// Build the ACP `AvailableCommand` list for the client autocomplete menu.
///
/// Skills include `scope` and `path` in `_meta` so the client can show
/// where the command comes from (e.g. "project" vs "global") and link
/// to the SKILL.md source.
pub(super) fn available_commands(
    skills: &[SkillInfo],
    availability: CommandAvailability,
    workflows: &[crate::session::workflow::registry::WorkflowListing],
) -> Vec<acp::AvailableCommand> {
    let catalog = EffectiveCommandCatalog::build(skills, availability, workflows);
    let mut commands =
        Vec::with_capacity(catalog.builtins.len() + catalog.skills.len() + catalog.workflows.len());
    commands.extend(catalog.builtins.iter().map(|builtin| {
        acp::AvailableCommand::new(builtin.name.to_string(), builtin.description.to_string()).input(
            builtin.argument_hint.map(|hint| {
                acp::AvailableCommandInput::Unstructured(acp::UnstructuredCommandInput::new(
                    hint.to_string(),
                ))
            }),
        )
    }));
    commands.extend(catalog.skills.iter().map(|command| {
        let skill = command.skill;
        let meta = serde_json::json!({"scope": skill.scope, "path": skill.path})
            .as_object()
            .cloned();
        acp::AvailableCommand::new(
            command.name.clone(),
            skill
                .short_description
                .as_deref()
                .unwrap_or(&skill.description)
                .to_string(),
        )
        .input(skill.argument_hint.as_ref().map(|hint| {
            acp::AvailableCommandInput::Unstructured(acp::UnstructuredCommandInput::new(
                hint.clone(),
            ))
        }))
        .meta(meta)
    }));
    commands.extend(catalog.workflows.iter().map(|workflow| {
        let meta = serde_json::json!({
            "workflowSource": workflow.source,
            "workflowPath": workflow.path,
        })
        .as_object()
        .cloned();
        acp::AvailableCommand::new(
            workflow.name.clone(),
            format!("Workflow: {}", workflow.description),
        )
        .input(Some(acp::AvailableCommandInput::Unstructured(
            acp::UnstructuredCommandInput::new("<args>".to_string()),
        )))
        .meta(meta)
    }));
    commands
}

/// Pre-session builtin commands for `InitializeResponse._meta`.
///
/// Advertises every always-on command plus any gated command whose gate
/// is satisfied by `availability`. Pre-session, only config-derived gates
/// (e.g. `goal`, which is driven by the `resolve_goal()` feature flag and
/// not by a live toolset) can be evaluated; runtime/tool-dependent gates
/// stay closed because there's no session context yet. See
/// `MvpAgent::command_availability` for how the pre-session snapshot is
/// built. With `CommandAvailability::default()` (all gates closed) this
/// is equivalent to advertising only `BuiltinGate::AlwaysOn` commands.
pub(crate) fn builtin_commands(availability: CommandAvailability) -> Vec<acp::AvailableCommand> {
    BUILTIN_COMMANDS
        .iter()
        .filter(|cmd| availability.allows(cmd.gate))
        .map(|cmd| {
            acp::AvailableCommand::new(cmd.name.to_string(), cmd.description.to_string()).input(
                cmd.argument_hint.map(|hint| {
                    acp::AvailableCommandInput::Unstructured(acp::UnstructuredCommandInput::new(
                        hint.to_string(),
                    ))
                }),
            )
        })
        .collect()
}

// ── x.ai/commands/list ext method ────────────────────────────────

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ListCommandsRequest {
    #[serde(default)]
    pub session_id: Option<acp::SessionId>,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(serde::Serialize)]
pub(crate) struct ListCommandsResponse {
    pub commands: Vec<acp::AvailableCommand>,
}

/// Build the available commands list, optionally scoped to a working directory.
/// - `Some(cwd)`: full skill discovery (Local + Repo + User) + builtins.
/// - `None`: builtins + global (User-scoped) skills only.
pub(crate) async fn list_commands(
    cwd: Option<&str>,
    skills_config: &xai_grok_agent::prompt::skills::SkillsConfig,
    plugin_registry: Option<&xai_grok_agent::plugins::PluginRegistry>,
    availability: CommandAvailability,
    compat: xai_grok_tools::types::compat::CompatConfig,
    include_project_workflows: bool,
) -> ListCommandsResponse {
    let skills = xai_grok_agent::prompt::skills::list_skills_with_plugins(
        cwd,
        skills_config,
        plugin_registry,
        compat,
    )
    .await;
    let workflows = crate::session::workflow::registry::list_workflows(
        include_project_workflows
            .then_some(cwd)
            .flatten()
            .map(std::path::Path::new),
    );
    ListCommandsResponse {
        commands: available_commands(&skills, availability, &workflows),
    }
}

// ── Slash command resolution ────────────────────────────────────

/// A parsed skill reference from user input.
///
/// Produced by `parse_skill_references()` when scanning user text for known
/// `/{skill_name}` tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedSkillRef {
    /// The skill name (bare or qualified, as typed by the user).
    pub name: String,
    /// Arguments following this skill reference, up to the next skill or end-of-input.
    pub args: String,
    /// The resolved `SkillInfo` path, for loading SKILL.md.
    pub skill_path: String,
    /// Scope-qualified name (e.g. "user:commit"), used for telemetry.
    pub qualified_name: String,
    /// Plugin name if this is a plugin skill.
    pub plugin_name: Option<String>,
}

#[derive(Debug)]
pub(super) enum SlashCommandOutcome {
    /// Execute directly, no model round-trip.
    Builtin(BuiltinAction),
    /// One or more skills detected in user input.
    ///
    /// The original prompt `blocks` are preserved verbatim — they are NOT
    /// rewritten. The shell's prompt assembly layer will read each skill's
    /// SKILL.md, apply substitutions, and build the `<skill_information>`
    /// envelope alongside the `<user_query>` block.
    InvokeSkill {
        /// The original, unmodified prompt blocks.
        blocks: Vec<acp::ContentBlock>,
        /// Parsed skill references (one per detected `/{skill}` token).
        skills: Vec<ParsedSkillRef>,
    },
}

#[derive(Debug)]
pub(super) enum BuiltinAction {
    Compact {
        user_context: Option<String>,
    },
    SetYolo {
        enabled: bool,
    },
    FlushMemory,
    Dream,
    ContextInfo,
    HooksTrust,
    HooksList,
    HooksAdd {
        path: String,
    },
    HooksRemove {
        path: String,
    },
    HooksUntrust,
    PluginsList,
    PluginsReload,
    PluginsTrust,
    SessionInfo,
    PluginsAdd {
        path: String,
    },
    PluginsRemove {
        path: String,
    },
    PluginsInstall {
        source: String,
        trust: bool,
    },
    PluginsUninstall {
        name: String,
        confirm: bool,
    },
    PluginsUpdate {
        name: Option<String>,
    },
    Feedback {
        text: String,
    },
    MemoryBrowse,
    MemoryToggle {
        enabled: bool,
    },
    GoalSet {
        objective: String,
        token_budget: Option<i64>,
    },
    GoalStatus,
    GoalPause,
    GoalResume,
    GoalClear,
    DeepResearch {
        query: String,
    },
    WorkflowManage {
        run_id: String,
        op: String,
    },
    WorkflowLaunch {
        name: String,
        input: String,
    },
}

impl BuiltinAction {
    pub(crate) fn command_name(&self) -> &'static str {
        match self {
            BuiltinAction::Compact { .. } => "compact",
            BuiltinAction::SetYolo { .. } => "yolo",
            BuiltinAction::FlushMemory => "flush",
            BuiltinAction::Dream => "dream",
            BuiltinAction::ContextInfo => "context",
            BuiltinAction::HooksTrust => "hooks-trust",
            BuiltinAction::HooksList => "hooks-list",
            BuiltinAction::HooksAdd { .. } => "hooks-add",
            BuiltinAction::HooksRemove { .. } => "hooks-remove",
            BuiltinAction::HooksUntrust => "hooks-untrust",
            BuiltinAction::PluginsList => "plugins-list",
            BuiltinAction::PluginsReload => "plugins-reload",
            BuiltinAction::PluginsTrust => "plugins-trust",
            BuiltinAction::SessionInfo => "session",
            BuiltinAction::PluginsAdd { .. } => "plugins-add",
            BuiltinAction::PluginsRemove { .. } => "plugins-remove",
            BuiltinAction::PluginsInstall { .. } => "plugins-install",
            BuiltinAction::PluginsUninstall { .. } => "plugins-uninstall",
            BuiltinAction::PluginsUpdate { .. } => "plugins-update",
            BuiltinAction::Feedback { .. } => "feedback",
            BuiltinAction::MemoryBrowse => "memory",
            BuiltinAction::MemoryToggle { .. } => "memory",
            BuiltinAction::GoalSet { .. }
            | BuiltinAction::GoalStatus
            | BuiltinAction::GoalPause
            | BuiltinAction::GoalResume
            | BuiltinAction::GoalClear => "goal",
            BuiltinAction::DeepResearch { .. } => "deep-research",
            BuiltinAction::WorkflowManage { .. } => "workflow",
            BuiltinAction::WorkflowLaunch { .. } => "workflow",
        }
    }

    pub(crate) fn args_provided(&self) -> bool {
        match self {
            BuiltinAction::Compact { user_context } => user_context.is_some(),
            BuiltinAction::SetYolo { .. } => true,
            BuiltinAction::FlushMemory => false,
            BuiltinAction::Dream => false,
            BuiltinAction::ContextInfo => false,
            BuiltinAction::HooksTrust => false,
            BuiltinAction::HooksList => false,
            BuiltinAction::HooksAdd { .. } => true,
            BuiltinAction::HooksRemove { .. } => true,
            BuiltinAction::HooksUntrust => false,
            BuiltinAction::PluginsList => false,
            BuiltinAction::PluginsReload => false,
            BuiltinAction::PluginsTrust => false,
            BuiltinAction::SessionInfo => false,
            BuiltinAction::PluginsAdd { .. } => true,
            BuiltinAction::PluginsRemove { .. } => true,
            BuiltinAction::PluginsInstall { .. } => true,
            BuiltinAction::PluginsUninstall { .. } => true,
            BuiltinAction::PluginsUpdate { name } => name.is_some(),
            BuiltinAction::Feedback { text } => !text.is_empty(),
            BuiltinAction::MemoryBrowse => false,
            BuiltinAction::MemoryToggle { .. } => true,
            BuiltinAction::GoalSet { .. } => true,
            BuiltinAction::GoalStatus
            | BuiltinAction::GoalPause
            | BuiltinAction::GoalResume
            | BuiltinAction::GoalClear => false,
            BuiltinAction::DeepResearch { .. } => true,
            BuiltinAction::WorkflowManage { .. } => true,
            BuiltinAction::WorkflowLaunch { input, .. } => !input.is_empty(),
        }
    }
}

/// How to rewrite the user's prompt when a slash command resolves to a skill.
///
/// - `RewriteToRun` (default): replace `/foo args` with `"run /foo args"`,
///   matching today's Grok Build flow that calls our dedicated `skill` tool.
/// - `Passthrough`: leave the prompt verbatim. Some templates use this —
///   the model is trained to spot a leading `/<name>`, look it up in the
///   `<agent_skills>` listing, and call the Read tool on `fullPath`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum SkillSlashRewrite {
    #[default]
    RewriteToRun,
    Passthrough,
}

/// Scan user input left-to-right for `/{word}` tokens where `word` matches
/// a **known registered skill name** (bare or qualified).
///
/// Unknown `/words` (like `/api/v2/users`, `/tmp/file`) are NOT treated as
/// skill references — only tokens that resolve to a known skill count.
///
/// Returns `None` when no known skill references are found. Otherwise returns
/// the list of `ParsedSkillRef` entries with each skill's args (the text
/// between one skill token and the next, or end-of-input).
pub(crate) fn parse_skill_references(
    text: &str,
    skills: &[SkillInfo],
    availability: CommandAvailability,
) -> Option<Vec<ParsedSkillRef>> {
    let catalog = EffectiveCommandCatalog::build(skills, availability, &[]);
    parse_skill_references_with_catalog(text, &catalog)
}

fn parse_skill_references_with_catalog(
    text: &str,
    catalog: &EffectiveCommandCatalog<'_>,
) -> Option<Vec<ParsedSkillRef>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    struct SkillHit<'a> {
        offset: usize,
        typed_name: String,
        skill: &'a SkillInfo,
    }
    let mut hits = Vec::new();
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'/' {
            i += 1;
            continue;
        }
        if i > 0 && !bytes[i - 1].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i + 1;
        if start >= bytes.len() {
            break;
        }
        let end = trimmed[start..]
            .find(|c: char| c.is_whitespace())
            .map(|relative| start + relative)
            .unwrap_or(trimmed.len());
        let word = &trimmed[start..end];
        if let Some(skill) = catalog.skill(word) {
            hits.push(SkillHit {
                offset: i,
                typed_name: word.to_string(),
                skill,
            });
        }
        i = end.max(start);
    }
    if hits.is_empty() {
        return None;
    }
    Some(
        hits.iter()
            .enumerate()
            .map(|(index, hit)| {
                let word_end = hit.offset + 1 + hit.typed_name.len();
                let args_end = hits
                    .get(index + 1)
                    .map(|next| next.offset)
                    .unwrap_or(trimmed.len());
                ParsedSkillRef {
                    name: hit.typed_name.clone(),
                    args: trimmed[word_end..args_end].trim().to_string(),
                    skill_path: hit.skill.path.clone(),
                    qualified_name: format_skill_name(hit.skill),
                    plugin_name: hit.skill.plugin_name.clone(),
                }
            })
            .collect(),
    )
}

/// Load each parsed skill's SKILL.md, apply substitutions, and build the
/// `<skill_information>` envelope.
///
/// Shared by turn start (prompt assembly in `process_conversation_turn`) and
/// the mid-turn interjection drain, so a skill delivers identically whether
/// it starts a turn or is force-sent into a running one. Returns `None` when
/// no skill content loads (missing files are logged and skipped; the
/// `<skills_referenced>` index still lists every parsed ref).
pub(super) async fn build_skill_information_for_refs(
    parsed_skills: &[ParsedSkillRef],
    slash_skills: &[SkillInfo],
    session_id: &str,
) -> Option<String> {
    use xai_grok_tools::implementations::skills::skill::{
        SkillRef, SubstitutionContext, apply_substitutions, build_skill_block,
        build_skill_information, load_skill_content,
    };

    let mut skill_blocks: Vec<String> = Vec::new();
    for sk in parsed_skills {
        // Find the SkillInfo by path (more reliable than by name for
        // qualified skills).
        let Some(info) = slash_skills.iter().find(|s| s.path == sk.skill_path) else {
            continue;
        };
        match load_skill_content(info).await {
            Ok(mut content) => {
                let skill_dir = std::path::Path::new(&info.path)
                    .parent()
                    .and_then(|p| p.to_str());
                let args = if sk.args.is_empty() {
                    None
                } else {
                    Some(sk.args.as_str())
                };
                apply_substitutions(
                    &mut content,
                    args,
                    &SubstitutionContext {
                        skill_dir,
                        session_id: Some(session_id),
                        plugin_root: info.plugin_root.as_deref(),
                        plugin_data: info.plugin_data.as_deref(),
                    },
                );
                skill_blocks.push(build_skill_block(&sk.name, &sk.args, &content));
            }
            Err(e) => {
                tracing::warn!(skill = %sk.name, error = %e, "failed to load skill for expansion");
            }
        }
    }

    if skill_blocks.is_empty() {
        return None;
    }
    let refs: Vec<SkillRef<'_>> = parsed_skills
        .iter()
        .map(|sk| SkillRef {
            name: &sk.name,
            path: &sk.skill_path,
        })
        .collect();
    Some(build_skill_information(&skill_blocks, &refs))
}

/// Resolve prompt blocks as a slash command.
/// `Ok(blocks)` = not a command, pass through. `Err(outcome)` = matched.
pub(super) fn resolve(
    prompt_blocks: Vec<acp::ContentBlock>,
    skills: &[SkillInfo],
    availability: CommandAvailability,
    _skill_rewrite: SkillSlashRewrite,
    workflows: &[crate::session::workflow::registry::WorkflowListing],
) -> Result<Vec<acp::ContentBlock>, SlashCommandOutcome> {
    let Some((command_name, args)) = parse_slash_prefix(&prompt_blocks) else {
        return Ok(prompt_blocks);
    };

    // Prompt-only commands (e.g. /loop) need a full agent round-trip, not
    // a direct BuiltinAction. They're filtered against the same gate the
    // PROMPT_COMMANDS entry declares -- looking it up here means the gate
    // value lives in exactly one place (the PROMPT_COMMANDS entry) and a
    // future addition just needs the entry, not a parallel branch.
    if let Some(prompt_cmd) = PROMPT_COMMANDS.iter().find(|c| c.name == command_name)
        && availability.allows(prompt_cmd.gate)
    {
        // Dispatch by name so a future PROMPT_COMMANDS entry without a
        // matching arm fails loudly at the call site instead of silently
        // reusing /loop's prompt builder.
        let mut blocks = match prompt_cmd.name {
            "loop" => build_loop_prompt_blocks(args),
            other => {
                unreachable!("prompt-only command /{other} has no resolver wired in resolve()")
            }
        };
        // Annotate with the compact invocation as `displayText` so every client
        // and session replay renders "/loop <args>" instead of the expanded
        // instruction. The pager does this client-side; bare-text clients rely
        // on this server-side annotation.
        let display_text = if args.is_empty() {
            format!("/{command_name}")
        } else {
            format!("/{command_name} {args}")
        };
        if let Some(acp::ContentBlock::Text(tb)) = blocks.first_mut() {
            let map = tb.meta.get_or_insert_with(acp::Meta::new);
            map.insert(
                "displayText".to_string(),
                serde_json::Value::String(display_text),
            );
        }
        // /loop is a prompt-only command — use InvokeSkill with empty skills
        // so the caller forwards the rewritten blocks directly to the model.
        return Err(SlashCommandOutcome::InvokeSkill {
            blocks,
            skills: vec![],
        });
    }

    let catalog = EffectiveCommandCatalog::build(skills, availability, workflows);
    if let Some(builtin) = catalog
        .builtins
        .iter()
        .find(|builtin| builtin.name == command_name || builtin.aliases.contains(&command_name))
    {
        let action = (builtin.resolve)(args);
        if matches!(action, BuiltinAction::WorkflowLaunch { .. }) && !availability.workflows {
            return Ok(prompt_blocks);
        }
        return Err(SlashCommandOutcome::Builtin(action));
    }

    // Not a builtin — use the multi-skill parser to detect ALL /{skill}
    // references in the full input text, splitting args at skill boundaries.
    let full_text = prompt_blocks
        .iter()
        .find_map(|b| {
            if let acp::ContentBlock::Text(t) = b {
                Some(t.text.as_str())
            } else {
                None
            }
        })
        .unwrap_or("");

    if let Some(parsed_skills) = parse_skill_references_with_catalog(full_text, &catalog) {
        return Err(SlashCommandOutcome::InvokeSkill {
            blocks: prompt_blocks,
            skills: parsed_skills,
        });
    }

    if let Some(workflow) = catalog.workflow(command_name) {
        return Err(SlashCommandOutcome::Builtin(
            BuiltinAction::WorkflowLaunch {
                name: workflow.name.clone(),
                input: args.to_string(),
            },
        ));
    }

    // No known skill matched — pass through as regular user input.
    Ok(prompt_blocks)
}

/// Extract `(name, args)` if the first text block starts with `/`.
///
/// - `"/compact keep auth"` → `Some(("compact", "keep auth"))`
/// - `"please run /commit"` → `None` (not at start)
fn parse_slash_prefix(prompt_blocks: &[acp::ContentBlock]) -> Option<(&str, &str)> {
    let text = prompt_blocks.iter().find_map(|b| {
        if let acp::ContentBlock::Text(t) = b {
            Some(t.text.as_str())
        } else {
            None
        }
    })?;

    let trimmed = text.trim();
    let without_slash = trimmed.strip_prefix('/')?;

    let (name, args) = match without_slash.find(char::is_whitespace) {
        Some(idx) => (&without_slash[..idx], without_slash[idx..].trim()),
        None => (without_slash, ""),
    };

    if name.is_empty() {
        return None;
    }

    Some((name, args))
}

/// Build the `/loop` prompt blocks for the shell client.
///
/// The wording (usage hint + scheduling instruction) is sourced from
/// `xai-grok-tools` so it stays identical to the pager's `LoopCommand` and the
/// two front-ends can't drift. Like the pager, there is no host-side interval
/// default: the model derives the cadence from the request and asks when none
/// is given.
fn build_loop_prompt_blocks(args: &str) -> Vec<acp::ContentBlock> {
    use xai_grok_tools::implementations::grok_build::{
        loop_schedule_instruction, loop_usage_message,
    };

    let text = if args.trim().is_empty() {
        loop_usage_message().to_string()
    } else {
        loop_schedule_instruction(args)
    };

    vec![acp::ContentBlock::Text(acp::TextContent::new(text))]
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_tools::implementations::skills::types::SkillScope;

    fn all_gated() -> CommandAvailability {
        CommandAvailability::all_enabled()
    }

    fn text_block(s: &str) -> acp::ContentBlock {
        acp::ContentBlock::Text(acp::TextContent::new(s.to_string()))
    }

    fn make_skill(name: &str, user_invocable: bool) -> SkillInfo {
        SkillInfo {
            name: name.to_string(),
            display_name: None,
            description: format!("A skill called {name}"),
            when_to_use: None,
            short_description: Some(format!("Short: {name}")),
            author: None,
            argument_hint: None,
            path: format!("/path/to/{name}/SKILL.md"),
            scope: SkillScope::Local,
            config_source: None,
            plugin_name: None,
            plugin_version: None,
            plugin_root: None,
            plugin_data: None,
            allowed_tools: None,
            license: None,
            compatibility: None,
            metadata: None,
            model: None,
            effort: None,
            user_invocable,
            disable_model_invocation: false,
            has_user_specified_description: false,
            paths: None,
            enabled: true,
            body: None,
        }
    }

    /// Extract the first parsed skill from an InvokeSkill outcome.
    fn first_skill(outcome: SlashCommandOutcome) -> ParsedSkillRef {
        match outcome {
            SlashCommandOutcome::InvokeSkill { skills, .. } => {
                assert!(!skills.is_empty(), "expected at least one skill");
                skills.into_iter().next().unwrap()
            }
            _ => panic!("expected InvokeSkill"),
        }
    }

    /// Extract original text from InvokeSkill blocks (for prompt-only commands like /loop).
    fn invoke_text(outcome: SlashCommandOutcome) -> String {
        match outcome {
            SlashCommandOutcome::InvokeSkill { blocks, .. } => blocks
                .iter()
                .find_map(|b| match b {
                    acp::ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .unwrap(),
            _ => panic!("expected InvokeSkill"),
        }
    }

    // ── parse_slash_prefix ──────────────────────────────────────────

    #[test]
    fn parse_slash_prefix_extracts_name_and_args() {
        assert_eq!(
            parse_slash_prefix(&[text_block("/compact keep auth")]),
            Some(("compact", "keep auth")),
        );
        assert_eq!(
            parse_slash_prefix(&[text_block("/yolo")]),
            Some(("yolo", "")),
        );
    }

    #[test]
    fn parse_slash_prefix_ignores_non_leading_slash() {
        assert_eq!(
            parse_slash_prefix(&[text_block("please run /commit")]),
            None
        );
        assert_eq!(parse_slash_prefix(&[text_block("fix the bug")]), None);
        assert_eq!(parse_slash_prefix(&[text_block("/")]), None);
    }

    #[test]
    fn parse_slash_prefix_trims_whitespace() {
        assert_eq!(
            parse_slash_prefix(&[text_block("  /commit fix typo  ")]),
            Some(("commit", "fix typo")),
        );
    }

    // ── builtin resolve fns ─────────────────────────────────────────

    fn resolve_builtin(name: &str, args: &str) -> Option<BuiltinAction> {
        BUILTIN_COMMANDS
            .iter()
            .chain(PROMPT_COMMANDS.iter())
            .find(|b| b.name == name)
            .map(|b| (b.resolve)(args))
    }

    #[test]
    fn compact_parses_optional_context() {
        assert!(matches!(
            resolve_builtin("compact", ""),
            Some(BuiltinAction::Compact { user_context: None })
        ));
        assert!(matches!(
            resolve_builtin("compact", "keep auth"),
            Some(BuiltinAction::Compact { user_context: Some(ctx) }) if ctx == "keep auth"
        ));
    }

    #[test]
    fn always_approve_parses_on_off() {
        for arg in ["", "on", "true", "1", "yes", "enable"] {
            assert!(
                matches!(
                    resolve_builtin("always-approve", arg),
                    Some(BuiltinAction::SetYolo { enabled: true })
                ),
                "expected on for {arg:?}",
            );
        }
        for arg in ["off", "false", "0", "no", "disable"] {
            assert!(
                matches!(
                    resolve_builtin("always-approve", arg),
                    Some(BuiltinAction::SetYolo { enabled: false })
                ),
                "expected off for {arg:?}",
            );
        }
    }

    #[test]
    fn yolo_alias_resolves_to_always_approve() {
        // /yolo should resolve via alias to the always-approve command
        let blocks = vec![text_block("/yolo on")];
        let outcome =
            resolve(blocks, &[], all_gated(), SkillSlashRewrite::default(), &[]).unwrap_err();
        assert!(matches!(
            outcome,
            SlashCommandOutcome::Builtin(BuiltinAction::SetYolo { enabled: true })
        ));
    }

    // ── resolve ─────────────────────────────────────────────────────

    #[test]
    fn resolve_routes_builtin() {
        let outcome = resolve(
            vec![text_block("/compact preserve auth")],
            &[],
            all_gated(),
            SkillSlashRewrite::default(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(
            outcome,
            SlashCommandOutcome::Builtin(BuiltinAction::Compact { user_context: Some(ctx) })
            if ctx == "preserve auth"
        ));
    }

    #[test]
    fn status_alias_resolves_to_session_info() {
        let outcome = resolve(
            vec![text_block("/status")],
            &[],
            all_gated(),
            SkillSlashRewrite::default(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(
            outcome,
            SlashCommandOutcome::Builtin(BuiltinAction::SessionInfo)
        ));
    }

    #[test]
    fn resolve_parses_skill_with_args() {
        let skills = vec![make_skill("commit", true)];
        let outcome = resolve(
            vec![text_block("/commit fix typo")],
            &skills,
            all_gated(),
            SkillSlashRewrite::default(),
            &[],
        )
        .unwrap_err();
        let skill = first_skill(outcome);
        assert_eq!(skill.name, "commit");
        assert_eq!(skill.args, "fix typo");

        let outcome = resolve(
            vec![text_block("/commit")],
            &skills,
            all_gated(),
            SkillSlashRewrite::default(),
            &[],
        )
        .unwrap_err();
        let skill = first_skill(outcome);
        assert_eq!(skill.name, "commit");
        assert_eq!(skill.args, "");
    }

    /// `build_skill_information_for_refs` loads the SKILL.md, applies
    /// substitutions, and wraps everything in `<skill_information>`;
    /// unloadable refs are skipped, and no loadable content → `None`.
    /// Shared by turn start and the interjection drain.
    #[tokio::test]
    async fn build_skill_information_for_refs_loads_and_wraps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        std::fs::write(&path, "Body with $ARGUMENTS").unwrap();

        let mut skill = make_skill("commit", true);
        skill.path = path.to_string_lossy().to_string();
        let skills = vec![skill];

        let parsed = parse_skill_references("/commit fix typo", &skills, all_gated())
            .expect("known skill must parse");
        let info = build_skill_information_for_refs(&parsed, &skills, "sid-1")
            .await
            .expect("skill body must load");
        assert!(info.starts_with("<skill_information>"), "got: {info}");
        assert!(
            info.contains("<skill name=\"commit\" args=\"fix typo\">"),
            "got: {info}"
        );
        assert!(
            info.contains("Body with fix typo"),
            "$ARGUMENTS must substitute: {info}"
        );

        // Missing file → logged, skipped, and with nothing loaded: None.
        let missing = vec![make_skill("ghost", true)];
        let parsed = parse_skill_references("/ghost", &missing, all_gated())
            .expect("known skill must parse");
        assert_eq!(
            build_skill_information_for_refs(&parsed, &missing, "sid-1").await,
            None
        );
    }

    #[test]
    fn resolve_loop_annotates_block_with_compact_display_text() {
        let outcome = resolve(
            vec![text_block("/loop 1m echo hello")],
            &[],
            all_gated(),
            SkillSlashRewrite::default(),
            &[],
        )
        .unwrap_err();
        let blocks = match outcome {
            SlashCommandOutcome::InvokeSkill { blocks, skills } => {
                assert!(skills.is_empty(), "/loop is a prompt-only command");
                blocks
            }
            _ => panic!("expected InvokeSkill for /loop"),
        };
        let acp::ContentBlock::Text(tb) = blocks.first().expect("one block") else {
            panic!("expected a text block");
        };
        assert!(
            tb.text.len() > "/loop 1m echo hello".len(),
            "wire text should be the expanded instruction"
        );
        let display = tb
            .meta
            .as_ref()
            .and_then(|m| m.get("displayText"))
            .and_then(|v| v.as_str());
        assert_eq!(display, Some("/loop 1m echo hello"));
        assert!(
            tb.meta
                .as_ref()
                .and_then(|m| m.get("displayAsSkill"))
                .is_none(),
            "/loop renders as a plain prompt, not a skill"
        );
    }

    #[test]
    fn resolve_loop_without_args_uses_bare_command_display_text() {
        // `/loop` with no args expands to the usage message but should still
        // carry a sensible compact `displayText`.
        let outcome = resolve(
            vec![text_block("/loop")],
            &[],
            all_gated(),
            SkillSlashRewrite::default(),
            &[],
        )
        .unwrap_err();
        let SlashCommandOutcome::InvokeSkill { blocks, .. } = outcome else {
            panic!("expected InvokeSkill for /loop");
        };
        let acp::ContentBlock::Text(tb) = blocks.first().expect("one block") else {
            panic!("expected a text block");
        };
        assert_eq!(
            tb.meta
                .as_ref()
                .and_then(|m| m.get("displayText"))
                .and_then(|v| v.as_str()),
            Some("/loop")
        );
    }

    #[test]
    fn resolve_passthrough_preserves_original_blocks() {
        // External-harness agents: blocks are passed through verbatim.
        // The prompt assembly layer decides how to format them.
        let skills = vec![make_skill("commit", true)];
        let outcome = resolve(
            vec![text_block("/commit fix typo")],
            &skills,
            all_gated(),
            SkillSlashRewrite::Passthrough,
            &[],
        )
        .unwrap_err();
        // Original text is preserved in blocks.
        assert_eq!(invoke_text(outcome), "/commit fix typo");

        let outcome = resolve(
            vec![text_block("/commit")],
            &skills,
            all_gated(),
            SkillSlashRewrite::Passthrough,
            &[],
        )
        .unwrap_err();
        assert_eq!(invoke_text(outcome), "/commit");
    }

    #[test]
    fn resolve_passes_through_normal_prompts() {
        let skills = vec![make_skill("commit", true)];
        assert!(
            resolve(
                vec![text_block("fix the login bug")],
                &skills,
                all_gated(),
                SkillSlashRewrite::default(),
                &[],
            )
            .is_ok()
        );
        assert!(
            resolve(
                vec![text_block("/unknown")],
                &skills,
                all_gated(),
                SkillSlashRewrite::default(),
                &[],
            )
            .is_ok()
        );
    }

    #[test]
    fn resolve_filters_non_invocable_skills() {
        let skills = vec![make_skill("internal-only", false)];
        assert!(
            resolve(
                vec![text_block("/internal-only")],
                &skills,
                all_gated(),
                SkillSlashRewrite::default(),
                &[],
            )
            .is_ok()
        );
    }

    #[test]
    fn resolve_builtin_shadows_same_named_skill() {
        let skills = vec![make_skill("compact", true)];
        let outcome = resolve(
            vec![text_block("/compact")],
            &skills,
            all_gated(),
            SkillSlashRewrite::default(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(outcome, SlashCommandOutcome::Builtin(_)));
    }

    // ── available_commands (ACP) ─────────────────────────────────────

    #[test]
    fn available_commands_orders_builtins_first() {
        let skills = vec![make_skill("commit", true), make_skill("deploy", true)];
        let commands = available_commands(&skills, all_gated(), &[]);
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "compact",
                "always-approve",
                "flush",
                "dream",
                "memory",
                "context",
                "hooks-trust",
                "hooks-list",
                "hooks-add",
                "hooks-remove",
                "hooks-untrust",
                "plugins",
                "reload-plugins",
                "session-info",
                "feedback",
                "deep-research",
                "workflow",
                "goal",
                "loop",
                "commit",
                "deploy",
            ]
        );
    }

    fn advertised_names(availability: CommandAvailability) -> Vec<String> {
        available_commands(&[], availability, &[])
            .into_iter()
            .map(|c| c.name)
            .collect()
    }

    #[test]
    fn availability_filters_memory_commands() {
        // memory=false hides /flush and /dream but NOT /memory (gated on
        // memory_configured instead, so the user can re-enable via toggle).
        let names = advertised_names(CommandAvailability {
            memory: false,
            ..CommandAvailability::all_enabled()
        });
        assert!(!names.iter().any(|n| n == "flush"), "got: {names:?}");
        assert!(!names.iter().any(|n| n == "dream"), "got: {names:?}");
        assert!(
            names.iter().any(|n| n == "memory"),
            "/memory should still be available when memory_configured=true, got: {names:?}"
        );
        assert!(names.iter().any(|n| n == "compact"));

        // memory_configured=false hides /memory too.
        let names2 = advertised_names(CommandAvailability {
            memory: false,
            memory_configured: false,
            ..CommandAvailability::all_enabled()
        });
        assert!(
            !names2.iter().any(|n| n == "memory"),
            "/memory should be hidden when memory_configured=false, got: {names2:?}"
        );
    }

    #[test]
    fn availability_filters_loop_command() {
        let names = advertised_names(CommandAvailability {
            scheduler: false,
            ..CommandAvailability::all_enabled()
        });
        assert!(!names.iter().any(|n| n == "loop"), "got: {names:?}");
    }

    #[test]
    fn workflows_gate_hides_workflow_but_not_goal() {
        let names = advertised_names(CommandAvailability {
            workflows: false,
            workflow_management: false,
            ..CommandAvailability::all_enabled()
        });
        assert!(!names.iter().any(|n| n == "workflow"), "got: {names:?}");
        assert!(names.iter().any(|n| n == "goal"), "got: {names:?}");

        let names2 = advertised_names(CommandAvailability {
            goal: false,
            ..CommandAvailability::all_enabled()
        });
        assert!(!names2.iter().any(|n| n == "goal"), "got: {names2:?}");
        assert!(names2.iter().any(|n| n == "workflow"), "got: {names2:?}");
    }

    #[test]
    fn availability_filters_hooks_and_plugins() {
        let names = advertised_names(CommandAvailability {
            hooks: false,
            plugins: false,
            ..CommandAvailability::all_enabled()
        });
        for n in [
            "hooks-trust",
            "hooks-list",
            "hooks-add",
            "hooks-remove",
            "hooks-untrust",
            "plugins",
            "reload-plugins",
        ] {
            assert!(
                !names.iter().any(|x| x == n),
                "{n} should be hidden, got: {names:?}",
            );
        }
    }

    #[test]
    fn availability_filters_goal_command() {
        let names = advertised_names(CommandAvailability {
            goal: false,
            ..CommandAvailability::all_enabled()
        });
        assert!(!names.iter().any(|n| n == "goal"), "got: {names:?}");
    }

    #[test]
    fn goal_does_not_resolve_when_host_capability_is_off() {
        let availability = CommandAvailability {
            goal: false,
            ..CommandAvailability::all_enabled()
        };
        assert!(
            resolve(
                vec![text_block("/goal status")],
                &[],
                availability,
                SkillSlashRewrite::default(),
                &[],
            )
            .is_ok(),
            "expected pass-through (Ok), got an outcome",
        );
    }

    #[test]
    fn loop_does_not_resolve_when_scheduler_unavailable() {
        // Without the scheduler gate the shell should not route /loop --
        // it would otherwise produce a useless "call scheduler_create"
        // prompt the model can't act on.
        let availability = CommandAvailability {
            scheduler: false,
            ..CommandAvailability::all_enabled()
        };
        assert!(
            resolve(
                vec![text_block("/loop 5m do thing")],
                &[],
                availability,
                SkillSlashRewrite::default(),
                &[],
            )
            .is_ok(),
            "expected pass-through (Ok), got an outcome",
        );
    }

    /// Extract the text of the first block produced by `build_loop_prompt_blocks`.
    fn loop_text(args: &str) -> String {
        match build_loop_prompt_blocks(args).into_iter().next() {
            Some(acp::ContentBlock::Text(t)) => t.text,
            other => panic!("expected a text block, got {other:?}"),
        }
    }

    #[test]
    fn loop_usage_has_no_10m_default() {
        // The shell client must not advertise a silent 10m default.
        let usage = loop_text("");
        assert!(usage.contains("Usage: /loop"), "got: {usage}");
        assert!(
            !usage.contains("10m"),
            "usage must not claim a default: {usage}"
        );
    }

    #[test]
    fn loop_instruction_derives_interval_without_default_or_inline_execute() {
        let instr = loop_text("every 30 minutes do x");
        assert!(
            !instr.contains("10m"),
            "instruction must not default: {instr}"
        );
        assert!(instr.contains("30 minutes"));
        assert!(instr.contains("<number><unit>"));
        assert!(instr.contains("ask the user how often"));
        assert!(instr.contains("Do NOT execute the prompt inline"));
        assert!(
            !instr.contains("immediately execute the parsed prompt"),
            "stale inline-execute wording must be gone: {instr}"
        );
        assert!(instr.contains("every 30 minutes do x"));
    }

    #[test]
    fn loop_prompt_matches_pager_wording() {
        // The shell and pager must stay textually identical so they don't drift.
        use xai_grok_tools::implementations::grok_build::{
            loop_schedule_instruction, loop_usage_message,
        };
        assert_eq!(loop_text(""), loop_usage_message());
        assert_eq!(
            loop_text("2h run tests"),
            loop_schedule_instruction("2h run tests")
        );
    }

    #[test]
    fn build_tools_meta_serialises_tool_names() {
        let names = vec!["scheduler_create".to_string(), "image_gen".to_string()];
        let v = build_tools_meta(&names);
        assert_eq!(
            serde_json::Value::Object(v),
            serde_json::json!({"tools": ["scheduler_create", "image_gen"]})
        );
    }

    #[test]
    fn pre_session_builtin_commands_excludes_gated_entries() {
        // The pre-session list (advertised in InitializeResponse._meta)
        // with a default (fail-closed) availability must not include any
        // gated command -- we don't know the toolset yet at that point.
        let names: Vec<String> = builtin_commands(CommandAvailability::default())
            .into_iter()
            .map(|c| c.name)
            .collect();
        for forbidden in [
            "flush",
            "dream",
            "memory",
            "feedback",
            "goal",
            "hooks-list",
            "plugins",
            "reload-plugins",
        ] {
            assert!(
                !names.iter().any(|n| n == forbidden),
                "{forbidden} should be excluded pre-session, got: {names:?}",
            );
        }
        // Always-on commands are still present.
        for required in ["compact", "always-approve", "context", "session-info"] {
            assert!(
                names.iter().any(|n| n == required),
                "{required} should be present, got: {names:?}",
            );
        }
    }

    #[test]
    fn pre_session_builtin_commands_advertises_goal_when_flag_enabled() {
        // `/goal` is gated on a config feature flag known at initialize
        // time (not a live toolset), so when the pre-session availability
        // enables it the command must be advertised -- otherwise it would
        // only show up after the first user turn created a session.
        let availability = CommandAvailability {
            goal: true,
            ..CommandAvailability::default()
        };
        let names: Vec<String> = builtin_commands(availability)
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert!(
            names.iter().any(|n| n == "goal"),
            "goal should be advertised pre-session when the flag is on, got: {names:?}",
        );
        // Runtime/tool-dependent gates stay closed pre-session.
        for forbidden in ["flush", "dream", "memory", "feedback", "plugins"] {
            assert!(
                !names.iter().any(|n| n == forbidden),
                "{forbidden} should stay excluded pre-session, got: {names:?}",
            );
        }
    }

    #[test]
    fn available_commands_populates_acp_fields() {
        let skills = vec![make_skill("commit", true)];
        let commands = available_commands(&skills, all_gated(), &[]);

        let builtin = commands.iter().find(|c| c.name == "compact").unwrap();
        assert!(builtin.input.is_some());

        let flush = commands.iter().find(|c| c.name == "flush").unwrap();
        assert!(flush.input.is_none()); // no argument_hint

        let skill = commands.iter().find(|c| c.name == "commit").unwrap();
        assert_eq!(skill.description, "Short: commit");
    }

    // ── /flush ─────────────────────────────────────────────────────

    #[test]
    fn flush_resolves_to_builtin_action() {
        assert!(matches!(
            resolve_builtin("flush", ""),
            Some(BuiltinAction::FlushMemory)
        ));
        // Args are ignored — still resolves to FlushMemory
        assert!(matches!(
            resolve_builtin("flush", "some extra args"),
            Some(BuiltinAction::FlushMemory)
        ));
    }

    #[test]
    fn resolve_routes_flush_builtin() {
        let outcome = resolve(
            vec![text_block("/flush")],
            &[],
            all_gated(),
            SkillSlashRewrite::default(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(
            outcome,
            SlashCommandOutcome::Builtin(BuiltinAction::FlushMemory)
        ));
    }

    #[test]
    fn flush_builtin_shadows_same_named_skill() {
        let skills = vec![make_skill("flush", true)];
        let outcome = resolve(
            vec![text_block("/flush")],
            &skills,
            all_gated(),
            SkillSlashRewrite::default(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(outcome, SlashCommandOutcome::Builtin(_)));
    }

    // ── /dream ─────────────────────────────────────────────────────

    #[test]
    fn dream_resolves_to_builtin_action() {
        assert!(matches!(
            resolve_builtin("dream", ""),
            Some(BuiltinAction::Dream)
        ));
        assert!(matches!(
            resolve_builtin("dream", "extra args"),
            Some(BuiltinAction::Dream)
        ));
    }

    #[test]
    fn resolve_routes_dream_builtin() {
        let outcome = resolve(
            vec![text_block("/dream")],
            &[],
            all_gated(),
            SkillSlashRewrite::default(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(
            outcome,
            SlashCommandOutcome::Builtin(BuiltinAction::Dream)
        ));
    }

    #[test]
    fn dream_builtin_shadows_same_named_skill() {
        let skills = vec![make_skill("dream", true)];
        let outcome = resolve(
            vec![text_block("/dream")],
            &skills,
            all_gated(),
            SkillSlashRewrite::default(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(outcome, SlashCommandOutcome::Builtin(_)));
    }

    // ── ambiguous skill names ─────────────────────────────────────

    fn make_scoped_skill(name: &str, scope: SkillScope) -> SkillInfo {
        SkillInfo {
            name: name.to_string(),
            display_name: None,
            description: format!("A {scope:?} skill called {name}"),
            when_to_use: None,
            short_description: Some(format!("Short: {name}")),
            author: None,
            argument_hint: None,
            path: format!("/path/to/{name}/{scope:?}/SKILL.md"),
            scope,
            config_source: None,
            plugin_name: None,
            plugin_version: None,
            plugin_root: None,
            plugin_data: None,
            allowed_tools: None,
            license: None,
            compatibility: None,
            metadata: None,
            model: None,
            effort: None,
            user_invocable: true,
            disable_model_invocation: false,
            has_user_specified_description: false,
            paths: None,
            enabled: true,
            body: None,
        }
    }

    #[test]
    fn resolve_ambiguous_bare_name_passes_through() {
        // Two skills share the bare name "commit" in different scopes.
        let skills = vec![
            make_scoped_skill("commit", SkillScope::Local),
            make_scoped_skill("commit", SkillScope::User),
        ];
        // Bare "/commit" is ambiguous -- should pass through (not first-match).
        assert!(
            resolve(
                vec![text_block("/commit")],
                &skills,
                all_gated(),
                SkillSlashRewrite::default(),
                &[],
            )
            .is_ok()
        );
    }

    #[test]
    fn resolve_qualified_skill_name() {
        let skills = vec![
            make_scoped_skill("commit", SkillScope::Local),
            make_scoped_skill("commit", SkillScope::User),
        ];

        // Qualified "/local:commit" resolves unambiguously.
        let outcome = resolve(
            vec![text_block("/local:commit fix typo")],
            &skills,
            all_gated(),
            SkillSlashRewrite::default(),
            &[],
        )
        .unwrap_err();
        let skill = first_skill(outcome);
        assert_eq!(skill.name, "local:commit");
        assert_eq!(skill.args, "fix typo");

        // Qualified "/user:commit" resolves unambiguously.
        let outcome = resolve(
            vec![text_block("/user:commit")],
            &skills,
            all_gated(),
            SkillSlashRewrite::default(),
            &[],
        )
        .unwrap_err();
        let skill = first_skill(outcome);
        assert_eq!(skill.name, "user:commit");
        assert_eq!(skill.args, "");
    }

    #[test]
    fn available_commands_uses_qualified_names_for_duplicates() {
        let skills = vec![
            make_scoped_skill("commit", SkillScope::Local),
            make_scoped_skill("commit", SkillScope::User),
            make_skill("deploy", true),
        ];
        let commands = available_commands(&skills, all_gated(), &[]);
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        // Duplicate "commit" skills should use qualified names.
        assert!(names.contains(&"local:commit"));
        assert!(names.contains(&"user:commit"));
        // Unique "deploy" keeps bare name only (no duplicate qualified form).
        assert!(names.contains(&"deploy"));
        assert!(
            !names.contains(&"local:deploy"),
            "non-colliding skill should NOT get a qualified duplicate, got: {names:?}"
        );
        // Bare "commit" should NOT appear.
        assert!(!names.contains(&"commit"));
    }

    // ── builtin/skill name collisions ─────────────────────────────

    #[test]
    fn available_commands_qualifies_builtin_colliding_skill() {
        // A skill named "compact" collides with the builtin /compact.
        let skills = vec![
            make_scoped_skill("compact", SkillScope::Local),
            make_skill("deploy", true),
        ];
        let commands = available_commands(&skills, all_gated(), &[]);
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        // The skill should be advertised under its qualified name.
        assert!(
            names.contains(&"local:compact"),
            "builtin-colliding skill should use qualified name, got: {names:?}"
        );
        // The bare "compact" entry should be the builtin, not the skill.
        let compact_cmd = commands.iter().find(|c| c.name == "compact").unwrap();
        assert!(
            compact_cmd.meta.is_none(),
            "bare 'compact' should be the builtin (no meta)"
        );
        // Non-colliding skill keeps bare name.
        assert!(names.contains(&"deploy"));
    }

    #[test]
    fn resolve_qualified_builtin_colliding_skill() {
        // A skill named "compact" collides with the builtin.
        let skills = vec![make_scoped_skill("compact", SkillScope::Local)];

        // Bare "/compact" should resolve to the builtin, not the skill.
        let outcome = resolve(
            vec![text_block("/compact")],
            &skills,
            all_gated(),
            SkillSlashRewrite::default(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(outcome, SlashCommandOutcome::Builtin(_)));

        // Qualified "/local:compact" should resolve to the skill.
        let outcome = resolve(
            vec![text_block("/local:compact")],
            &skills,
            all_gated(),
            SkillSlashRewrite::default(),
            &[],
        )
        .unwrap_err();
        let skill = first_skill(outcome);
        assert_eq!(skill.name, "local:compact");
        assert_eq!(skill.args, "");
    }

    #[test]
    fn feedback_does_not_resolve_when_disabled() {
        // /feedback should pass through as unrecognized when the feature is off.
        assert!(
            resolve(
                vec![text_block("/feedback hello")],
                &[],
                CommandAvailability::default(),
                SkillSlashRewrite::default(),
                &[],
            )
            .is_ok()
        );
    }

    #[test]
    fn feedback_resolves_when_enabled() {
        let outcome = resolve(
            vec![text_block("/feedback hello")],
            &[],
            all_gated(),
            SkillSlashRewrite::default(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(
            outcome,
            SlashCommandOutcome::Builtin(BuiltinAction::Feedback { ref text }) if text == "hello"
        ));
    }

    /// Collect the advertised command names for the given availability.
    fn advertised_names_with(availability: CommandAvailability) -> Vec<String> {
        available_commands(&[], availability, &[])
            .into_iter()
            .map(|c| c.name)
            .collect()
    }

    /// `CommandAvailability::default()` must be fail-closed: every gated
    /// command is hidden, only `BuiltinGate::AlwaysOn` survives. The
    /// pre-session `MvpAgent::command_availability()` builds on this value
    /// (only flipping config-derived gates like `goal` on), so a
    /// regression here would re-expose `/flush`, `/loop`, etc. on the home
    /// screen for harnesses that won't actually run them.
    #[test]
    fn default_availability_is_fail_closed_on_every_gate() {
        let names = advertised_names_with(CommandAvailability::default());
        for forbidden in [
            "flush",
            "dream",
            "feedback",
            "goal",
            "loop",
            "hooks-list",
            "hooks-trust",
            "hooks-untrust",
            "hooks-add",
            "hooks-remove",
            "plugins",
            "reload-plugins",
        ] {
            assert!(
                !names.iter().any(|n| n == forbidden),
                "{forbidden} must not be advertised under default fail-closed availability, got: {names:?}",
            );
        }
        for required in ["compact", "always-approve", "context", "session-info"] {
            assert!(
                names.iter().any(|n| n == required),
                "AlwaysOn {required} must always be advertised, got: {names:?}",
            );
        }
    }

    /// `/flush` is a memory-write that's only useful when the model can
    /// later read back what it wrote. The shell's
    /// `build_command_availability()` ANDs `memory.is_enabled()` with
    /// `memory_search`/`memory_get` registration; the gate itself just
    /// reads `availability.memory`. Lock both halves so a future change
    /// to either side is forced through this test.
    #[test]
    fn flush_hidden_when_memory_gate_off_visible_when_on() {
        let off = advertised_names_with(CommandAvailability::default());
        assert!(!off.iter().any(|n| n == "flush"), "got: {off:?}");
        assert!(!off.iter().any(|n| n == "dream"), "got: {off:?}");
        // /memory is gated on memory_configured, not memory — hidden here
        // because Default sets both to false.
        assert!(!off.iter().any(|n| n == "memory"), "got: {off:?}");

        let on = advertised_names_with(CommandAvailability {
            memory: true,
            memory_configured: true,
            ..CommandAvailability::default()
        });
        assert!(on.iter().any(|n| n == "flush"), "got: {on:?}");
        assert!(on.iter().any(|n| n == "dream"), "got: {on:?}");
        assert!(on.iter().any(|n| n == "memory"), "got: {on:?}");
    }

    // ── /memory ─────────────────────────────────────────────────────

    #[test]
    fn memory_bare_resolves_to_browse() {
        assert!(matches!(
            resolve_builtin("memory", ""),
            Some(BuiltinAction::MemoryBrowse)
        ));
        // Any unrecognized arg also falls through to browse
        assert!(matches!(
            resolve_builtin("memory", "status"),
            Some(BuiltinAction::MemoryBrowse)
        ));
    }

    #[test]
    fn memory_on_off_resolves_to_toggle() {
        for (arg, expected) in [
            ("on", true),
            ("enable", true),
            ("ON", true),
            ("Enable", true),
            ("off", false),
            ("disable", false),
            ("OFF", false),
            ("Disable", false),
        ] {
            assert!(
                matches!(
                    resolve_builtin("memory", arg),
                    Some(BuiltinAction::MemoryToggle { enabled }) if enabled == expected
                ),
                "expected toggle({expected}) for {arg:?}",
            );
        }
    }

    #[test]
    fn mem_alias_resolves_to_memory_browse() {
        let outcome = resolve(
            vec![text_block("/mem")],
            &[],
            all_gated(),
            SkillSlashRewrite::default(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(
            outcome,
            SlashCommandOutcome::Builtin(BuiltinAction::MemoryBrowse)
        ));
    }

    #[test]
    fn mem_alias_resolves_toggle_with_args() {
        let outcome = resolve(
            vec![text_block("/mem off")],
            &[],
            all_gated(),
            SkillSlashRewrite::default(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(
            outcome,
            SlashCommandOutcome::Builtin(BuiltinAction::MemoryToggle { enabled: false })
        ));
    }

    #[test]
    fn memory_resolves_when_disabled_but_configured() {
        // memory=false but memory_configured=true: /memory must still work
        // so the user can re-enable via the toggle.
        let availability = CommandAvailability {
            memory: false,
            ..CommandAvailability::all_enabled()
        };
        let outcome = resolve(
            vec![text_block("/memory")],
            &[],
            availability,
            SkillSlashRewrite::default(),
            &[],
        );
        assert!(
            outcome.is_err(),
            "expected /memory to resolve when memory_configured=true",
        );
    }

    #[test]
    fn memory_not_resolved_when_not_configured() {
        let availability = CommandAvailability {
            memory: false,
            memory_configured: false,
            ..CommandAvailability::all_enabled()
        };
        assert!(
            resolve(
                vec![text_block("/memory")],
                &[],
                availability,
                SkillSlashRewrite::default(),
                &[],
            )
            .is_ok(),
            "expected pass-through (Ok) when memory_configured is false",
        );
    }

    // ── parse_skill_references ──────────────────────────────────────

    #[test]
    fn parse_skill_refs_single_skill() {
        let skills = vec![make_skill("commit", true)];
        let refs = parse_skill_references("/commit fix typo", &skills, all_gated()).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "commit");
        assert_eq!(refs[0].args, "fix typo");
    }

    #[test]
    fn parse_skill_refs_single_no_args() {
        let skills = vec![make_skill("commit", true)];
        let refs = parse_skill_references("/commit", &skills, all_gated()).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "commit");
        assert_eq!(refs[0].args, "");
    }

    #[test]
    fn parse_skill_refs_multi_skill() {
        let skills = vec![make_skill("review", true), make_skill("lint", true)];
        let refs = parse_skill_references("/review fix auth /lint --strict", &skills, all_gated())
            .unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].name, "review");
        assert_eq!(refs[0].args, "fix auth");
        assert_eq!(refs[1].name, "lint");
        assert_eq!(refs[1].args, "--strict");
    }

    #[test]
    fn parse_skill_refs_ignores_unknown_slash() {
        let skills = vec![make_skill("commit", true)];
        // /api/v2/users is not a known skill — should be ignored.
        let result = parse_skill_references("check /api/v2/users", &skills, all_gated());
        assert!(result.is_none());
    }

    #[test]
    fn parse_skill_refs_ignores_builtins() {
        let skills = vec![make_skill("commit", true)];
        // /compact is a builtin — should NOT appear in skill refs.
        let result = parse_skill_references("/compact", &skills, all_gated());
        assert!(result.is_none());
    }

    #[test]
    fn parse_skill_refs_empty_text() {
        let skills = vec![make_skill("commit", true)];
        assert!(parse_skill_references("", &skills, all_gated()).is_none());
    }

    #[test]
    fn parse_skill_refs_no_slash() {
        let skills = vec![make_skill("commit", true)];
        assert!(parse_skill_references("just some text", &skills, all_gated()).is_none());
    }

    #[test]
    fn parse_skill_refs_non_invocable_skill_ignored() {
        let skills = vec![make_skill("internal-only", false)];
        assert!(parse_skill_references("/internal-only", &skills, all_gated()).is_none());
    }

    #[test]
    fn parse_skill_refs_qualified_name() {
        let skills = vec![
            make_scoped_skill("commit", SkillScope::Local),
            make_scoped_skill("commit", SkillScope::User),
        ];
        let refs = parse_skill_references("/local:commit fix typo", &skills, all_gated()).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "local:commit");
        assert_eq!(refs[0].args, "fix typo");
        assert_eq!(refs[0].qualified_name, "local:commit");
    }

    #[test]
    fn parse_skill_refs_text_before_first_skill() {
        // Text before the first skill reference is part of user query,
        // not consumed as args.
        let skills = vec![make_skill("commit", true)];
        let refs =
            parse_skill_references("please do /commit fix typo", &skills, all_gated()).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "commit");
        assert_eq!(refs[0].args, "fix typo");
    }

    // ── /goal command resolution ─────────────────────────────────

    fn resolve_goal(args: &str) -> BuiltinAction {
        let blocks = vec![text_block(&format!("/goal {args}"))];
        match resolve(blocks, &[], all_gated(), SkillSlashRewrite::default(), &[]).unwrap_err() {
            SlashCommandOutcome::Builtin(action) => action,
            _ => panic!("expected Builtin outcome"),
        }
    }

    #[test]
    fn goal_empty_resolves_to_status() {
        assert!(matches!(resolve_goal(""), BuiltinAction::GoalStatus));
    }

    fn listing(name: &str) -> crate::session::workflow::registry::WorkflowListing {
        crate::session::workflow::registry::WorkflowListing {
            name: name.to_string(),
            description: "does things".to_string(),
            when_to_use: None,
            source: "project",
            path: Some(format!(".grok/workflows/{name}.rhai")),
        }
    }

    #[test]
    fn named_workflows_advertise_and_resolve() {
        let workflows = vec![listing("triage-flakes"), listing("goal")];
        let commands = available_commands(&[], all_gated(), &workflows);
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"triage-flakes"), "{names:?}");
        assert_eq!(names.iter().filter(|n| **n == "goal").count(), 1);
        let wf = commands.iter().find(|c| c.name == "triage-flakes").unwrap();
        assert!(
            wf.description.starts_with("Workflow:"),
            "{}",
            wf.description
        );

        let blocks = vec![text_block("/triage-flakes fix the CI")];
        match resolve(
            blocks,
            &[],
            all_gated(),
            SkillSlashRewrite::default(),
            &workflows,
        )
        .unwrap_err()
        {
            SlashCommandOutcome::Builtin(BuiltinAction::WorkflowLaunch { name, input }) => {
                assert_eq!(name, "triage-flakes");
                assert_eq!(input, "fix the CI");
            }
            other => panic!("expected WorkflowLaunch, got {other:?}"),
        }

        let blocks = vec![text_block("/goal status")];
        assert!(matches!(
            resolve(
                blocks,
                &[],
                all_gated(),
                SkillSlashRewrite::default(),
                &workflows
            )
            .unwrap_err(),
            SlashCommandOutcome::Builtin(BuiltinAction::GoalStatus)
        ));
    }

    #[test]
    fn workflow_collision_policy_includes_aliases_and_ambiguous_skills() {
        let skills = vec![
            make_scoped_skill("commit", SkillScope::Local),
            make_scoped_skill("commit", SkillScope::User),
        ];
        let workflows = vec![
            listing("status"),
            listing("yolo"),
            listing("sessions"),
            listing("commit"),
            listing("review"),
        ];
        let names: Vec<_> = available_commands(&skills, all_gated(), &workflows)
            .into_iter()
            .map(|command| command.name)
            .collect();
        assert!(!names.iter().any(|name| name == "status"));
        assert!(!names.iter().any(|name| name == "yolo"));
        assert!(!names.iter().any(|name| name == "sessions"));
        assert!(!names.iter().any(|name| name == "commit"));
        assert!(names.iter().any(|name| name == "local:commit"));
        assert!(names.iter().any(|name| name == "user:commit"));
        assert!(names.iter().any(|name| name == "review"));

        assert!(matches!(
            resolve(
                vec![text_block("/status")],
                &skills,
                all_gated(),
                SkillSlashRewrite::default(),
                &workflows,
            )
            .unwrap_err(),
            SlashCommandOutcome::Builtin(BuiltinAction::SessionInfo)
        ));
        for unavailable in ["sessions", "commit"] {
            assert!(
                resolve(
                    vec![text_block(&format!("/{unavailable}"))],
                    &skills,
                    all_gated(),
                    SkillSlashRewrite::default(),
                    &workflows,
                )
                .is_ok()
            );
        }
    }

    #[test]
    fn duplicate_qualified_skills_are_omitted_and_do_not_first_match() {
        let mut first = make_scoped_skill("commit", SkillScope::Plugin);
        first.plugin_name = Some("same-plugin".into());
        let mut second = first.clone();
        second.path = "/other/commit/SKILL.md".into();
        let skills = vec![first, second];
        assert!(
            available_commands(&skills, all_gated(), &[])
                .iter()
                .all(|command| command.name != "same-plugin:commit")
        );
        assert!(
            resolve(
                vec![text_block("/same-plugin:commit")],
                &skills,
                all_gated(),
                SkillSlashRewrite::default(),
                &[],
            )
            .is_ok()
        );
    }

    #[test]
    fn existing_runs_keep_management_but_hide_launch_catalog() {
        let availability = CommandAvailability {
            workflows: false,
            workflow_management: true,
            ..CommandAvailability::all_enabled()
        };
        let workflows = vec![listing("review")];
        let names: Vec<_> = available_commands(&[], availability, &workflows)
            .into_iter()
            .map(|command| command.name)
            .collect();
        assert!(names.iter().any(|name| name == "workflow"));
        assert!(!names.iter().any(|name| name == "review"));
        assert!(!names.iter().any(|name| name == "deep-research"));
        assert!(matches!(
            resolve(
                vec![text_block("/workflow stop old-run")],
                &[],
                availability,
                SkillSlashRewrite::default(),
                &workflows,
            )
            .unwrap_err(),
            SlashCommandOutcome::Builtin(BuiltinAction::WorkflowManage { .. })
        ));
        assert!(
            resolve(
                vec![text_block("/workflow review")],
                &[],
                availability,
                SkillSlashRewrite::default(),
                &workflows,
            )
            .is_ok()
        );
    }

    #[test]
    fn workflow_manage_parses_both_orders_and_optional_id() {
        let resolve_workflow = |args: &str| -> BuiltinAction {
            let blocks = vec![text_block(&format!("/workflow {args}"))];
            match resolve(blocks, &[], all_gated(), SkillSlashRewrite::default(), &[]).unwrap_err()
            {
                SlashCommandOutcome::Builtin(action) => action,
                _ => panic!("expected Builtin outcome"),
            }
        };
        for (args, want_id, want_op) in [
            ("resume", "", "resume"),
            ("pause", "", "pause"),
            ("wf_12ab pause", "wf_12ab", "pause"),
            ("pause wf_12ab", "wf_12ab", "pause"),
            ("SAVE wf_12ab", "wf_12ab", "save"),
            ("pause deep research", "deep research", "pause"),
            ("", "", ""),
        ] {
            match resolve_workflow(args) {
                BuiltinAction::WorkflowManage { run_id, op } => {
                    assert_eq!(run_id, want_id, "args: {args:?}");
                    assert_eq!(op, want_op, "args: {args:?}");
                }
                other => panic!("expected WorkflowManage, got {}", other.command_name()),
            }
        }

        for (args, want_name, want_input) in [
            (
                r#"pr-review {"pr": 243776}"#,
                "pr-review",
                r#"{"pr": 243776}"#,
            ),
            ("pr-review", "pr-review", ""),
            (
                "deep-research rust pitfalls",
                "deep-research",
                "rust pitfalls",
            ),
            (
                "triage resume the failed jobs",
                "triage",
                "resume the failed jobs",
            ),
        ] {
            match resolve_workflow(args) {
                BuiltinAction::WorkflowLaunch { name, input } => {
                    assert_eq!(name, want_name, "args: {args:?}");
                    assert_eq!(input, want_input, "args: {args:?}");
                }
                other => panic!(
                    "expected WorkflowLaunch for {args:?}, got {}",
                    other.command_name()
                ),
            }
        }
    }

    #[test]
    fn goal_status_keyword_resolves_to_status() {
        assert!(matches!(resolve_goal("status"), BuiltinAction::GoalStatus));
        assert!(matches!(resolve_goal("STATUS"), BuiltinAction::GoalStatus));
    }

    #[test]
    fn goal_pause_resolves_to_pause() {
        assert!(matches!(resolve_goal("pause"), BuiltinAction::GoalPause));
        assert!(matches!(resolve_goal("PAUSE"), BuiltinAction::GoalPause));
    }

    #[test]
    fn goal_resume_resolves_to_resume() {
        assert!(matches!(resolve_goal("resume"), BuiltinAction::GoalResume));
    }

    #[test]
    fn goal_clear_resolves_to_clear() {
        assert!(matches!(resolve_goal("clear"), BuiltinAction::GoalClear));
    }

    #[test]
    fn goal_objective_resolves_to_set() {
        match resolve_goal("implement auth module") {
            BuiltinAction::GoalSet {
                objective,
                token_budget,
            } => {
                assert_eq!(objective, "implement auth module");
                assert_eq!(token_budget, None);
            }
            other => panic!("expected GoalSet, got {}", other.command_name()),
        }
    }

    #[test]
    fn goal_set_preserves_original_casing() {
        match resolve_goal("Fix BUG in AuthManager") {
            BuiltinAction::GoalSet { objective, .. } => {
                assert_eq!(objective, "Fix BUG in AuthManager");
            }
            other => panic!("expected GoalSet, got {}", other.command_name()),
        }
    }

    #[test]
    fn goal_set_trailing_budget_flag_parses() {
        match resolve_goal("implement X --budget 500000") {
            BuiltinAction::GoalSet {
                objective,
                token_budget,
            } => {
                assert_eq!(objective, "implement X");
                assert_eq!(token_budget, Some(500_000));
            }
            other => panic!("expected GoalSet, got {}", other.command_name()),
        }
    }

    #[test]
    fn goal_set_budget_accepts_boundary_and_extra_whitespace() {
        for (text, objective, budget) in [
            ("do x --budget 1", "do x", 1),
            ("do x --budget   77", "do x", 77),
            ("do x \t --budget 500000", "do x", 500_000),
        ] {
            match resolve_goal(text) {
                BuiltinAction::GoalSet {
                    objective: o,
                    token_budget,
                } => {
                    assert_eq!(o, objective);
                    assert_eq!(token_budget, Some(budget), "for {text:?}");
                }
                other => panic!("expected GoalSet, got {}", other.command_name()),
            }
        }
    }

    #[test]
    fn goal_set_malformed_budget_stays_in_objective() {
        // Non-numeric, missing, non-positive, glued, signed, overflowing,
        // or mid-text values must not be consumed as a budget.
        for text in [
            "implement X --budget abc",
            "implement X --budget",
            "implement X --budget 0",
            "implement X --budget -5",
            "implement X --budget +5",
            "implement X --budget 99999999999999999999",
            "implement X --budget5",
            "implement X --budget500000",
            "tune my-fund--budget 100",
            "fix the --budget flag parsing bug",
            "--budget 500000",
        ] {
            match resolve_goal(text) {
                BuiltinAction::GoalSet {
                    objective,
                    token_budget,
                } => {
                    assert_eq!(objective, text, "objective must be preserved verbatim");
                    assert_eq!(token_budget, None, "no budget must be parsed from {text:?}");
                }
                other => panic!("expected GoalSet, got {}", other.command_name()),
            }
        }
    }

    #[test]
    fn goal_command_name_is_goal() {
        assert_eq!(BuiltinAction::GoalStatus.command_name(), "goal");
        assert_eq!(BuiltinAction::GoalPause.command_name(), "goal");
        assert_eq!(BuiltinAction::GoalResume.command_name(), "goal");
        assert_eq!(BuiltinAction::GoalClear.command_name(), "goal");
        assert_eq!(
            BuiltinAction::GoalSet {
                objective: "x".into(),
                token_budget: None,
            }
            .command_name(),
            "goal"
        );
    }

    #[test]
    fn goal_args_provided() {
        assert!(
            BuiltinAction::GoalSet {
                objective: "x".into(),
                token_budget: None,
            }
            .args_provided()
        );
        assert!(!BuiltinAction::GoalStatus.args_provided());
        assert!(!BuiltinAction::GoalPause.args_provided());
        assert!(!BuiltinAction::GoalResume.args_provided());
        assert!(!BuiltinAction::GoalClear.args_provided());
    }

    // ── GoalTracker handler-level interaction tests ──────────────
    // These test the exact tracker state transitions that the slash
    // command handlers perform, without constructing a full SessionActor.

    #[test]
    fn goal_tracker_status_with_no_goal_returns_none() {
        use crate::session::goal_tracker::GoalTracker;
        let tracker = GoalTracker::new(std::path::PathBuf::from("/tmp/test"));
        assert!(tracker.snapshot().is_none());
        assert!(tracker.status().is_none());
    }

    #[test]
    fn goal_tracker_create_sets_active() {
        use crate::session::goal_tracker::{GoalStatus, GoalTracker};
        let mut tracker = GoalTracker::new(std::path::PathBuf::from("/tmp/test"));
        tracker.create_goal("g1".into(), "obj".into(), None, 0, "now".into(), None);
        assert_eq!(tracker.status(), Some(GoalStatus::Active));
        assert_eq!(tracker.objective(), Some("obj"));
    }

    #[test]
    fn goal_tracker_pause_only_when_active() {
        use crate::session::goal_tracker::{GoalPauseReason, GoalStatus, GoalTracker};
        let mut tracker = GoalTracker::new(std::path::PathBuf::from("/tmp/test"));
        // No goal — pause returns false
        assert!(!tracker.pause(GoalPauseReason::User));

        tracker.create_goal("g1".into(), "obj".into(), None, 0, "now".into(), None);
        assert!(tracker.pause(GoalPauseReason::User));
        assert_eq!(tracker.status(), Some(GoalStatus::UserPaused));
        // Already paused — pause returns false
        assert!(!tracker.pause(GoalPauseReason::User));
    }

    #[test]
    fn goal_tracker_resume_only_when_paused() {
        use crate::session::goal_tracker::{GoalPauseReason, GoalStatus, GoalTracker};
        let mut tracker = GoalTracker::new(std::path::PathBuf::from("/tmp/test"));
        tracker.create_goal("g1".into(), "obj".into(), None, 0, "now".into(), None);
        // Active — resume returns false
        assert!(!tracker.resume());
        tracker.pause(GoalPauseReason::User);
        assert!(tracker.resume());
        assert_eq!(tracker.status(), Some(GoalStatus::Active));
    }

    #[test]
    fn goal_tracker_clear_removes_orchestration() {
        use crate::session::goal_tracker::GoalTracker;
        let mut tracker = GoalTracker::new(std::path::PathBuf::from("/tmp/test"));
        tracker.create_goal("g1".into(), "obj".into(), None, 0, "now".into(), None);
        assert!(tracker.snapshot().is_some());
        tracker.clear();
        assert!(tracker.snapshot().is_none());
    }

    #[test]
    fn goal_tracker_create_replaces_existing() {
        use crate::session::goal_tracker::GoalTracker;
        let mut tracker = GoalTracker::new(std::path::PathBuf::from("/tmp/test"));
        tracker.create_goal("g1".into(), "first".into(), None, 0, "now".into(), None);
        tracker.create_goal("g2".into(), "second".into(), None, 0, "now".into(), None);
        assert_eq!(tracker.objective(), Some("second"));
    }

    #[test]
    fn goal_tracker_account_elapsed_flushes_delta() {
        use crate::session::goal_tracker::GoalTracker;
        let mut tracker = GoalTracker::new(std::path::PathBuf::from("/tmp/test"));
        tracker.create_goal("g1".into(), "obj".into(), None, 0, "now".into(), None);
        // After create_goal, elapsed_ms is 0 but active_since is set.
        let before = tracker.snapshot().unwrap().elapsed_ms;
        assert_eq!(before, 0);
        // account_elapsed flushes pending wall-clock time.
        std::thread::sleep(std::time::Duration::from_millis(5));
        tracker.account_elapsed();
        let after = tracker.snapshot().unwrap().elapsed_ms;
        assert!(after > 0, "elapsed should be > 0 after account_elapsed");
    }
}
