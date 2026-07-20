//! New, exit, cloud, and worktree session dispatchers plus trust and startup actions.
use super::fork::{dispatch_startup_fork_session, worktree_persist_options};
use super::load::dispatch_load_session;
use super::modal::remove_agent_and_cleanup;
use crate::acp::model_state::{EffortTokenError, ModelState};
use crate::acp::tracker::AcpUpdateTracker;
use crate::app::actions::{Action, Effect, SwitchModelError};
use crate::app::agent::{AgentCommand, AgentId, AgentSession, AgentState};
use crate::app::agent_view::{ActivePane, AgentView, McpInitProgress};
use crate::app::app_view::{ActiveView, AppView, TrustState};
use crate::app::dispatch::ctx::{
    SwitchCause, get_active_agent, reseed_tip_for_new_session, show_welcome, switch_to_agent,
};
use crate::app::dispatch::modes::inherit_auto_mode;
use crate::app::dispatch::prompt::{consume_chat_kind, dispatch_initial_prompt};
use crate::app::dispatch::queue::{QueueDrain, maybe_drain_queue, note_peek_page_flip};
use crate::app::dispatch::router::dispatch;
use crate::app::dispatch::status::notify_session_ready;
use crate::app::dispatch::task_result::unregister_session_effect;
use crate::app::dispatch::transcript::extensions_modal_tab_fetches;
use crate::scrollback::block::RenderBlock;
use crate::scrollback::blocks::SessionEvent;
use crate::scrollback::state::ScrollbackState;
use agent_client_protocol as acp;
use std::time::Instant;
use xai_grok_shell::sampling::types::ReasoningEffort;
/// A deferred model switch to apply once the session exists, plus any effort
/// error to surface. `switch` is still populated when a `-m` model was stashed
/// even if the effort token failed, so an invalid effort never drops the CLI
/// model override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeferredSwitchOutcome {
    pub switch: Option<(acp::ModelId, Option<ReasoningEffort>)>,
    pub effort_error: Option<EffortTokenError>,
}
/// Resolve the stashed `-m` switch and/or `cli_effort_token` against the session
/// catalog via [`ModelState::resolve_effort_for_model`] (same gate-first policy
/// as `/effort` and headless).
pub(crate) fn take_deferred_model_switch(
    stashed: Option<(acp::ModelId, Option<ReasoningEffort>)>,
    models: &ModelState,
    cli_effort_token: Option<&str>,
) -> DeferredSwitchOutcome {
    if let Some((model_id, mut effort)) = stashed {
        let effort_error = match cli_effort_token {
            Some(token) if effort.is_none() => {
                match models.resolve_effort_for_model(&model_id, token) {
                    Ok(resolved) => {
                        effort = Some(resolved);
                        None
                    }
                    Err(err) => Some(err),
                }
            }
            _ => None,
        };
        return DeferredSwitchOutcome {
            switch: Some((model_id, effort)),
            effort_error,
        };
    }
    let Some(token) = cli_effort_token else {
        return DeferredSwitchOutcome {
            switch: None,
            effort_error: None,
        };
    };
    let Some(current) = models.current.clone() else {
        return DeferredSwitchOutcome {
            switch: None,
            effort_error: Some(EffortTokenError::NoActiveModel),
        };
    };
    match models.resolve_effort_for_model(&current, token) {
        Ok(effort) if models.reasoning_effort == Some(effort) => DeferredSwitchOutcome {
            switch: None,
            effort_error: None,
        },
        Ok(effort) => DeferredSwitchOutcome {
            switch: Some((current, Some(effort))),
            effort_error: None,
        },
        Err(err) => DeferredSwitchOutcome {
            switch: None,
            effort_error: Some(err),
        },
    }
}
/// Take the stashed deferred switch, resolve any CLI effort token against the
/// session catalog, surface effort errors, and return the switch to apply.
pub(crate) fn apply_deferred_model_switch(
    agent: &mut AgentView,
    cli_effort_token: Option<&str>,
) -> Option<(acp::ModelId, Option<ReasoningEffort>)> {
    let stashed = agent.session.deferred_model_switch.take();
    let outcome = take_deferred_model_switch(stashed, &agent.session.models, cli_effort_token);
    apply_deferred_switch_outcome(agent, outcome)
}
/// Surface effort-token errors and return the model/effort switch (if any).
pub(crate) fn apply_deferred_switch_outcome(
    agent: &mut AgentView,
    outcome: DeferredSwitchOutcome,
) -> Option<(acp::ModelId, Option<ReasoningEffort>)> {
    if let Some(err) = outcome.effort_error {
        let msg = format!("--effort/--reasoning-effort: {}", err.message());
        tracing::warn!("{msg}");
        agent.show_toast(&msg);
        agent.scrollback.push_block(RenderBlock::system(msg));
    }
    outcome.switch
}
/// Top-level `/new` dispatcher. If the working directory is inside a
/// git repository, consults the persisted `new_session_worktree_mode`
/// preference: `Always` skips the popup and creates a worktree, `Never`
/// skips the popup and stays in-cwd, `Ask` opens the worktree question
/// modal (mirroring `/fork`'s UX). Otherwise proceeds directly via
/// [`dispatch_new_session_inner`].
///
/// When no active agent exists (e.g. from the welcome screen), the
/// git-repo check falls back to `app.cwd_has_git_ancestor` so the
/// persisted `Always` / `Never` modes still take effect. The `Ask`
/// path falls through to `dispatch_new_session_inner` in that case
/// because `open_new_session_question` requires an active agent.
pub(in crate::app::dispatch) fn dispatch_new_session(app: &mut AppView) -> Vec<Effect> {
    use crate::app::app_view::WorktreeMode;
    if !app.session_startup_allowed() {
        app.deferred_startup.new_session = true;
        return vec![];
    }
    let in_git_repo = get_active_agent(app)
        .map(|a| a.current_branch.is_some())
        .unwrap_or(app.cwd_has_git_ancestor);
    if in_git_repo {
        match app.new_session_worktree_mode {
            WorktreeMode::Always => {
                dispatch_new_worktree_session(app, None, None, None, None, None, None)
            }
            WorktreeMode::Never => dispatch_new_session_inner(app, None),
            WorktreeMode::Ask => {
                if matches!(app.active_view, ActiveView::Agent(_)) {
                    open_new_session_question(app)
                } else {
                    dispatch_new_session_inner(app, None)
                }
            }
        }
    } else {
        dispatch_new_session_inner(app, None)
    }
}
/// Open the local worktree question modal for `/new`. Mirrors
/// [`open_fork_question`] but uses [`LocalQuestionKind::NewSession`] so
/// the answer routes to [`dispatch_new_session_inner`] or
/// [`dispatch_new_worktree_session`].
pub(in crate::app::dispatch) fn open_new_session_question(app: &mut AppView) -> Vec<Effect> {
    use crate::views::question_view::{LocalQuestionKind, QuestionViewState};
    use xai_grok_tools::implementations::grok_build::ask_user_question::{
        Question, QuestionOption,
    };
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    if agent.question_view.is_some() {
        app.show_toast("Finish answering the current question first");
        return vec![];
    }
    let mut options = vec![
        QuestionOption {
            label: "Yes".into(),
            description: "New session in a new isolated git worktree".into(),
            preview: None,
            id: None,
        },
        QuestionOption {
            label: "No".into(),
            description: "New session in the current cwd".into(),
            preview: None,
            id: None,
        },
    ];
    options.extend(worktree_persist_options());
    let question = Question {
        question: "Start the new session in an isolated git worktree?".into(),
        id: None,
        options,
        multi_select: Some(false),
    };
    let agent = app.agents.get_mut(&id).expect("agent present (re-borrow)");
    let stashed = agent.prompt.stash();
    let state = QuestionViewState::new(
        format!("new-session-{}", uuid::Uuid::new_v4()),
        vec![question],
        stashed,
    )
    .with_local_kind(LocalQuestionKind::NewSession)
    .with_no_freeform();
    agent.question_view = Some(state);
    agent.prompt.set_text("");
    vec![]
}
/// Open a local QuestionView asking whether to start a new session for a
/// model that requires a different agent harness. Mirrors
/// [`open_new_session_question`] but uses
/// [`LocalQuestionKind::AgentTypeMismatch`] so the answer routes to
/// [`dispatch_new_session_inner`] with a deferred model switch.
pub(in crate::app::dispatch) fn open_agent_type_mismatch_question(
    app: &mut AppView,
    model_id: acp::ModelId,
    effort: Option<xai_grok_shell::sampling::types::ReasoningEffort>,
    model_name: &str,
) -> Vec<Effect> {
    use crate::views::question_view::{LocalQuestionKind, QuestionViewState};
    use xai_grok_tools::implementations::grok_build::ask_user_question::{
        Question, QuestionOption,
    };
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    if agent.question_view.is_some() {
        app.show_toast("Finish answering the current question first");
        return vec![];
    }
    let question = Question {
        question: format!("Switching to {model_name} requires starting a new session. Continue?"),
        id: None,
        options: vec![
            QuestionOption {
                label: "Yes".into(),
                description: format!("Start a new session with {model_name}"),
                preview: None,
                id: None,
            },
            QuestionOption {
                label: "No".into(),
                description: "Continue the current session".into(),
                preview: None,
                id: None,
            },
        ],
        multi_select: Some(false),
    };
    let agent = app.agents.get_mut(&id).expect("agent present (re-borrow)");
    let stashed = agent.prompt.stash();
    let state = QuestionViewState::new(
        format!("agent-type-mismatch-{}", uuid::Uuid::new_v4()),
        vec![question],
        stashed,
    )
    .with_local_kind(LocalQuestionKind::AgentTypeMismatch { model_id, effort })
    .with_no_freeform();
    agent.question_view = Some(state);
    agent.prompt.set_text("");
    vec![]
}
/// Core new-session logic: create a placeholder agent, push the
/// `/dashboard` tip, and return the `CreateSession` effect.
///
/// Factored out of [`dispatch_new_session`] so the worktree-question
/// "No" path can call it directly without re-opening the modal.
pub(in crate::app::dispatch) fn dispatch_new_session_inner(
    app: &mut AppView,
    model_id: Option<acp::ModelId>,
) -> Vec<Effect> {
    let (_id, effects) = dispatch_new_session_inner_with_id(app, model_id);
    effects
}
/// Sibling that returns the new `AgentId` alongside
/// the effects. Used by `dispatch_dashboard_dispatch` so it doesn't
/// rely on the (correct-but-brittle) `app.agents.last()` lookup.
pub(in crate::app::dispatch) fn dispatch_new_session_inner_with_id(
    app: &mut AppView,
    model_id: Option<acp::ModelId>,
) -> (AgentId, Vec<Effect>) {
    let mut effects =
        unregister_session_effect(get_active_agent(app).and_then(|a| a.session.session_id.clone()));
    let (effective_cwd, inherit_worktree) = get_active_agent(app)
        .map(|a| (a.session.cwd.clone(), a.session.is_worktree))
        .unwrap_or_else(|| (app.cwd.clone(), false));
    reseed_tip_for_new_session(app);
    let agent_id = AgentId(app.next_agent_id);
    app.next_agent_id += 1;
    let mut scrollback = ScrollbackState::new();
    scrollback.set_appearance(app.appearance.clone());
    let agent = AgentView::new(
        AgentSession {
            id: agent_id,
            acp_tx: app.acp_tx.clone(),
            session_id: None,
            models: app.models.clone(),
            state: AgentState::Idle,
            tracker: AcpUpdateTracker::new(),
            cwd: effective_cwd.clone(),
            is_worktree: inherit_worktree,
            forked_from: None,
            pending_prompts: std::collections::VecDeque::new(),
            next_queue_id: 0,
            yolo_mode: app.default_yolo,
            auto_mode: inherit_auto_mode(app),
            prompt_history: Vec::new(),
            prompt_history_loading: false,
            loading_replay: false,
            restore_degree: None,
            rate_limited: false,
            model_incompatible: false,
            credit_limit_blocked: false,
            free_usage_blocked: false,
            available_commands: app.bootstrap_acp_commands.clone(),
            available_commands_generation: 1,
            available_tools: None,
            model_switch_pending: false,
            user_model_preference: None,
            deferred_model_switch: app.deferred_model_switch_from_cli(),
            bg_tasks: std::collections::BTreeMap::new(),
            bg_tool_call_to_task: std::collections::HashMap::new(),
            scheduled_tasks: std::collections::HashMap::new(),
            in_flight_prompt: None,
            current_prompt_id: None,
            created_via_new: true,
        },
        scrollback,
    );
    app.agents.insert(agent_id, agent);
    {
        let agent = app.agents.get_mut(&agent_id).unwrap();
        agent.prompt.set_compact(app.appearance.prompt.compact);
        agent.prompt.adopt_slash_mru(app.slash_mru.clone());
        agent
            .prompt
            .set_contextual_hints(app.contextual_hints.undo, app.contextual_hints.plan_mode);
        agent.set_session_recap_available(app.session_recap_available);
        agent.set_voice_mode_available(app.voice_mode_enabled);
        agent.apply_app_scoped_gates(
            app.sharing_enabled,
            app.usage_visible,
            app.chat_mode,
            app.screen_mode,
            &app.active_announcements,
            &app.tier_restricted_commands,
        );
        agent.apply_credit_balance(app.credit_balance.clone(), app.auto_topup.clone());
        agent
            .prompt
            .slash_controller
            .registry_mut()
            .set_plugins_visible(!app.appearance.disable_plugins);
        agent.active_pane = ActivePane::Prompt;
    }
    switch_to_agent(app, agent_id, SwitchCause::New);
    if app.screen_mode.is_minimal() {
        app.minimal_state.welcome_pending = true;
    }
    if !app.needs_project_picker() {
        let chat_kind = consume_chat_kind(app);
        if let Some(agent) = app.agents.get_mut(&agent_id) {
            agent.chat_kind = chat_kind;
            agent.apply_credit_balance(app.credit_balance.clone(), app.auto_topup.clone());
            agent.mcp_init_progress = Some(McpInitProgress {
                total: 0,
                connected: 0,
                started_at: Instant::now(),
            });
            agent.session.prompt_history_loading = true;
        }
        let preferred_session_id = app.deferred_startup.preferred_session_id.take();
        effects.push(Effect::CreateSession {
            agent_id,
            cwd: effective_cwd,
            model_id,
            preferred_session_id,
            chat_kind,
        });
    }
    (agent_id, effects)
}
/// Exit the current session and return to the welcome screen.
pub(in crate::app::dispatch) fn dispatch_exit_session(app: &mut AppView) -> Vec<Effect> {
    let effects =
        unregister_session_effect(get_active_agent(app).and_then(|a| a.session.session_id.clone()));
    show_welcome(app);
    app.welcome_prompt_focused = true;
    app.session_picker_entries = None;
    app.session_picker_loading = false;
    app.session_picker_state.selected = 0;
    app.session_picker_content_results = None;
    app.session_picker_content_loading = false;
    app.exit_session_pending = None;
    effects
}
/// Handle the user accepting the folder-trust question: persist the grant for
/// the workspace (writes `~/.grok/trusted_folders.toml`), mark trust resolved,
/// then replay any deferred session startup (only if auth is also done).
pub(in crate::app::dispatch) fn dispatch_trust_folder(app: &mut AppView) -> Vec<Effect> {
    if let TrustState::Pending { workspace } = &app.trust_state {
        xai_grok_workspace::folder_trust::grant_folder_trust(workspace);
    }
    finish_trust(app)
}
/// Tail of accepting the folder-trust question (via [`dispatch_trust_folder`];
/// declining quits instead): resolve `trust_state` to `Done`, focus the welcome
/// prompt, and replay the deferred session startup once auth is also resolved.
/// Drains iff [`AppView::session_startup_allowed`] -- the same predicate
/// `AuthComplete` uses, so whichever gate resolves last drains exactly once.
pub(in crate::app::dispatch) fn finish_trust(app: &mut AppView) -> Vec<Effect> {
    app.trust_state = TrustState::Done;
    app.welcome_prompt_focused = !app.is_access_blocked();
    if app.session_startup_allowed() {
        drain_startup_actions(app)
    } else {
        vec![]
    }
}
/// Clear EVERY deferred `startup_*` action without replaying any. Used on the
/// paths that must not run startup at all — ZDR-blocked login, and mid-session
/// re-auth (`auth_return_view`) where a session already exists — so a stash
/// (including an incidental mid-session `Ctrl+N` that the chokepoint deferred)
/// can never linger and fire later. Atomic counterpart to `drain_startup_actions`.
pub(in crate::app::dispatch) fn clear_startup_actions(app: &mut AppView) {
    let _ = app.deferred_startup.take();
}
/// Replay the session-startup actions deferred until auth + trust both resolved
/// (`--resume` / `--worktree` / initial-prompt / `grok dashboard`). Extracted
/// from the `AuthComplete` handler so the folder-trust answer can run the SAME
/// machinery; whichever gate resolves last drains it (each call site guards on
/// the other gate being `Done`, so it runs exactly once).
pub(in crate::app::dispatch) fn drain_startup_actions(app: &mut AppView) -> Vec<Effect> {
    use crate::app::session_startup::{DeferredSessionStartup, DeferredStartupActions};
    debug_assert!(
        app.session_startup_allowed(),
        "drain_startup_actions must run only with the startup gate open"
    );
    let DeferredStartupActions {
        session: deferred,
        preferred_session_id: preferred_id,
        worktree,
        worktree_label,
        worktree_ref,
        new_session,
        prompt,
        open_dashboard,
        pending_chat,
    } = app.deferred_startup.take();
    let mut effects = Vec::new();
    match deferred {
        Some(DeferredSessionStartup::Fork {
            parent_session_id,
            parent_cwd,
            new_session_id,
        }) => {
            effects.extend(dispatch_startup_fork_session(
                app,
                parent_session_id,
                parent_cwd,
                new_session_id,
            ));
        }
        Some(DeferredSessionStartup::Load {
            session_id,
            session_cwd,
            chat_kind,
        }) => {
            if worktree {
                if chat_kind || pending_chat {
                    app.deferred_startup.pending_chat = true;
                }
                effects.extend(dispatch_new_worktree_session(
                    app,
                    Some(session_id),
                    worktree_label,
                    None,
                    None,
                    worktree_ref,
                    None,
                ));
            } else {
                effects.extend(dispatch_load_session(
                    app,
                    session_id,
                    session_cwd,
                    chat_kind,
                ));
            }
        }
        Some(DeferredSessionStartup::NewWithId { session_id }) => {
            if pending_chat {
                app.deferred_startup.pending_chat = true;
            }
            if worktree {
                effects.extend(dispatch_new_worktree_session(
                    app,
                    None,
                    worktree_label,
                    None,
                    None,
                    worktree_ref,
                    Some(session_id),
                ));
            } else {
                effects.extend(dispatch_new_session_with_id(app, session_id));
            }
        }
        Some(DeferredSessionStartup::ForeignResume { tool, native_id }) => {
            effects.extend(dispatch_new_session_inner(app, None));
            effects.extend(dispatch(
                Action::SendPrompt(
                    crate::app::foreign_sessions::ForeignPickerSource::from_tool(tool)
                        .resume_prompt(&native_id),
                ),
                app,
            ));
        }
        None => {
            if pending_chat {
                app.deferred_startup.pending_chat = true;
            }
            if let Some(sid) = preferred_id {
                if worktree {
                    effects.extend(dispatch_new_worktree_session(
                        app,
                        None,
                        worktree_label,
                        None,
                        None,
                        worktree_ref,
                        Some(sid),
                    ));
                } else {
                    effects.extend(dispatch_new_session_with_id(app, sid));
                }
            } else if worktree {
                effects.extend(dispatch_new_worktree_session(
                    app,
                    None,
                    worktree_label,
                    None,
                    None,
                    worktree_ref,
                    None,
                ));
            } else if new_session {
                effects.extend(dispatch_new_session(app));
            } else {
                app.deferred_startup.pending_chat = false;
            }
        }
    }
    if let Some(prompt) = prompt {
        effects.extend(dispatch_initial_prompt(app, prompt));
    }
    if open_dashboard {
        effects.extend(dispatch(Action::OpenDashboard, app));
    }
    effects
}
/// Create a placeholder agent in a new git worktree, then create a session.
pub(in crate::app::dispatch) fn dispatch_new_worktree_session(
    app: &mut AppView,
    load_session_id: Option<String>,
    label: Option<String>,
    prompt: Option<String>,
    model_id: Option<acp::ModelId>,
    git_ref: Option<String>,
    preferred_session_id: Option<String>,
) -> Vec<Effect> {
    let preferred_session_id =
        preferred_session_id.or_else(|| app.deferred_startup.preferred_session_id.take());
    if !app.session_startup_allowed() {
        debug_assert!(
            prompt.is_none() && model_id.is_none(),
            "worktree prompt/model_id only flow from an active session (gate open)"
        );
        if let Some(sid) = load_session_id {
            app.deferred_startup.session =
                Some(crate::app::session_startup::DeferredSessionStartup::Load {
                    session_id: sid,
                    session_cwd: None,
                    chat_kind: app.deferred_startup.pending_chat,
                });
        }
        app.deferred_startup.worktree = true;
        if label.is_some() {
            app.deferred_startup.worktree_label = label;
        }
        if git_ref.is_some() {
            app.deferred_startup.worktree_ref = git_ref;
        }
        if preferred_session_id.is_some() {
            app.deferred_startup.preferred_session_id = preferred_session_id;
        }
        return vec![];
    }
    if !app.cwd_has_git_ancestor {
        let msg: String = "Not inside a git repository. Navigate to a git repo \
                      or run 'git init' first."
            .into();
        if !app.startup_warnings.iter().any(|w| w.message == msg) {
            app.startup_warnings.push(crate::startup::StartupWarning {
                severity: crate::startup::WarningSeverity::Warning,
                message: msg,
                action: None,
            });
        }
        return vec![];
    }
    if load_session_id.is_none() {
        reseed_tip_for_new_session(app);
    }
    let agent_id = AgentId(app.next_agent_id);
    app.next_agent_id += 1;
    let mut scrollback = ScrollbackState::new();
    scrollback.set_appearance(app.appearance.clone());
    let mut agent = AgentView::new(
        AgentSession {
            id: agent_id,
            acp_tx: app.acp_tx.clone(),
            session_id: None,
            models: app.models.clone(),
            state: AgentState::Idle,
            tracker: AcpUpdateTracker::new(),
            cwd: app.cwd.clone(),
            is_worktree: false,
            forked_from: None,
            pending_prompts: std::collections::VecDeque::new(),
            next_queue_id: 0,
            yolo_mode: app.default_yolo,
            auto_mode: inherit_auto_mode(app),
            prompt_history: Vec::new(),
            prompt_history_loading: false,
            loading_replay: false,
            restore_degree: None,
            rate_limited: false,
            model_incompatible: false,
            credit_limit_blocked: false,
            free_usage_blocked: false,
            available_commands: app.bootstrap_acp_commands.clone(),
            available_commands_generation: 1,
            available_tools: None,
            model_switch_pending: false,
            user_model_preference: None,
            deferred_model_switch: app.deferred_model_switch_from_cli(),
            bg_tasks: std::collections::BTreeMap::new(),
            bg_tool_call_to_task: std::collections::HashMap::new(),
            scheduled_tasks: std::collections::HashMap::new(),
            in_flight_prompt: None,
            current_prompt_id: None,
            created_via_new: false,
        },
        scrollback,
    );
    let cmd = if load_session_id.is_some() {
        AgentCommand::RestoreWorktree
    } else {
        AgentCommand::CreateWorktree
    };
    agent.session.start_command(cmd);
    agent.turn_started_at = Some(Instant::now());
    app.agents.insert(agent_id, agent);
    let chat_kind = if load_session_id.is_none() {
        consume_chat_kind(app)
    } else {
        app.deferred_startup.pending_chat
    };
    {
        let agent = app.agents.get_mut(&agent_id).unwrap();
        agent.prompt.set_compact(app.appearance.prompt.compact);
        agent.prompt.adopt_slash_mru(app.slash_mru.clone());
        agent
            .prompt
            .set_contextual_hints(app.contextual_hints.undo, app.contextual_hints.plan_mode);
        agent.set_session_recap_available(app.session_recap_available);
        agent.set_voice_mode_available(app.voice_mode_enabled);
        agent.apply_app_scoped_gates(
            app.sharing_enabled,
            app.usage_visible,
            app.chat_mode,
            app.screen_mode,
            &app.active_announcements,
            &app.tier_restricted_commands,
        );
        agent.chat_kind = chat_kind;
        agent.apply_credit_balance(app.credit_balance.clone(), app.auto_topup.clone());
        agent
            .prompt
            .slash_controller
            .registry_mut()
            .set_plugins_visible(!app.appearance.disable_plugins);
    }
    if let Some(prompt) = prompt
        && let Some(agent) = app.agents.get_mut(&agent_id)
    {
        agent.session.enqueue_prompt(prompt);
    }
    switch_to_agent(app, agent_id, SwitchCause::New);
    let effects = vec![Effect::CreateWorktreeSession {
        agent_id,
        load_session_id,
        label,
        git_ref,
        model_id,
        preferred_session_id,
        chat_kind,
    }];
    effects
}
pub(in crate::app::dispatch) fn dispatch_new_session_with_id(
    app: &mut AppView,
    session_id: String,
) -> Vec<Effect> {
    app.deferred_startup.preferred_session_id = Some(session_id.clone());
    if !app.session_startup_allowed() {
        app.deferred_startup.session =
            Some(crate::app::session_startup::DeferredSessionStartup::NewWithId { session_id });
        return vec![];
    }
    let (agent_id, mut effects) = dispatch_new_session_inner_with_id(app, None);
    for e in &mut effects {
        if let Effect::CreateSession {
            preferred_session_id,
            ..
        } = e
        {
            *preferred_session_id = Some(session_id.clone());
        }
    }
    let _ = agent_id;
    effects
}
/// Tear down a placeholder agent that must not proceed under sticky `--chat`
/// (local Build refuse). Never leave a half-loaded slot with a bound session id.
pub(in crate::app::dispatch) fn refuse_chat_mode_build_agent(app: &mut AppView, agent_id: AgentId) {
    app.show_toast(crate::app::session_startup::CHAT_MODE_LOCAL_BUILD_REFUSAL);
    let fallback = app.agents.keys().copied().find(|id| *id != agent_id);
    remove_agent_and_cleanup(app, agent_id);
    if let Some(target) = fallback {
        switch_to_agent(app, target, SwitchCause::Picker);
    } else {
        show_welcome(app);
        app.welcome_prompt_focused = true;
        app.session_picker_entries = None;
        app.session_picker_loading = false;
        app.session_picker_state.selected = 0;
        app.session_picker_content_results = None;
        app.session_picker_content_loading = false;
        let msg = crate::app::session_startup::CHAT_MODE_LOCAL_BUILD_REFUSAL.to_string();
        if !app.startup_warnings.iter().any(|w| w.message == msg) {
            app.startup_warnings.push(crate::startup::StartupWarning {
                severity: crate::startup::WarningSeverity::Warning,
                message: msg,
                action: None,
            });
        }
    }
}
/// Dismiss the project picker and create a session in the current directory.
pub(in crate::app::dispatch) fn skip_picker_and_create_session(
    app: &mut AppView,
    agent_id: AgentId,
) -> Vec<Effect> {
    if app
        .agents
        .get(&agent_id)
        .is_some_and(|a| a.session.session_id.is_some() || a.mcp_init_progress.is_some())
    {
        return vec![];
    }
    app.mark_project_picker_done();
    let chat_kind = consume_chat_kind(app);
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        agent.chat_kind = chat_kind;
        agent.apply_credit_balance(app.credit_balance.clone(), app.auto_topup.clone());
        agent.mcp_init_progress = Some(McpInitProgress {
            total: 0,
            connected: 0,
            started_at: Instant::now(),
        });
        agent.session.prompt_history_loading = true;
        if let Some(qv) = agent.question_view.take() {
            agent.prompt.restore(qv.stashed_prompt);
        }
    }
    let preferred_session_id = app.deferred_startup.preferred_session_id.take();
    vec![Effect::CreateSession {
        agent_id,
        cwd: app.cwd.clone(),
        model_id: None,
        preferred_session_id,
        chat_kind,
    }]
}
pub(in crate::app::dispatch) fn handle_session_created(
    app: &mut AppView,
    agent_id: AgentId,
    session_id: acp::SessionId,
    new_models: Option<acp::SessionModelState>,
) -> Vec<Effect> {
    let agent_count = app.agents.len();
    let switch_hint =
        crate::views::dashboard::session_switch_hint_command(app.screen_mode.is_minimal());
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        let session_id_clone = session_id.clone();
        if agent.session.created_via_new
            && agent_count > 1
            && let Some(cmd) = switch_hint
        {
            agent.scrollback.push_block(RenderBlock::system(format!(
                "Session {} \u{2014} use {cmd} to switch between sessions",
                session_id_clone.0,
            )));
        } else if agent_count > 1 {
            agent.scrollback.push_block(RenderBlock::system(format!(
                "Session: {}",
                session_id_clone.0,
            )));
        }
        agent.bind_session_id(session_id);
        if let Some(m) = new_models {
            app.models = Some(m).into();
            agent.session.models = app.models.clone();
        }
        let deferred = apply_deferred_model_switch(agent, app.cli_effort_token.as_deref());
        let deferred_mode = agent.deferred_session_mode.take();
        let cwd = agent.session.cwd.clone();
        if deferred.is_some() {
            agent.session.model_switch_pending = true;
        }
        let mut drain = if app.reconnect_pending {
            QueueDrain {
                effects: vec![],
                page_flip_entry: None,
            }
        } else {
            maybe_drain_queue(agent)
        };
        let mut effects = std::mem::take(&mut drain.effects);
        agent.session.prompt_history_loading = true;
        effects.push(Effect::FetchPromptHistory {
            agent_id,
            cwd: cwd.clone(),
            session_id: session_id_clone.to_string(),
        });
        effects.push(Effect::FetchSessionAgentName {
            agent_id,
            session_id: session_id_clone.clone(),
        });
        effects.push(Effect::RefreshAvailableCommands { agent_id, cwd });
        effects.push(Effect::CheckMarketplaceUpdates {
            agent_id,
            session_id: session_id_clone.clone(),
        });
        if app.plugin_cta_enabled {
            effects.push(Effect::FetchPluginCtaCatalog {
                agent_id,
                session_id: session_id_clone.clone(),
            });
        }
        effects.push(Effect::FetchBilling {
            agent_id,
            silent: true,
        });
        if let Some((model_id, effort)) = deferred {
            effects.push(Effect::SwitchModel {
                agent_id,
                session_id: session_id_clone.clone(),
                model_id,
                effort,
                prev_model_id: None,
            });
        }
        if let Some(mode) = deferred_mode {
            effects.push(Effect::SetSessionMode {
                session_id: session_id_clone.clone(),
                mode_id: acp::SessionModeId::new(mode.as_id()),
            });
        }
        if std::mem::take(&mut agent.pending_extensions_fetch) && agent.extensions_modal.is_some() {
            effects.extend(extensions_modal_tab_fetches(
                agent_id,
                session_id_clone.clone(),
            ));
        }
        effects.push(Effect::RegisterActiveSession {
            session_id: session_id_clone,
            cwd: agent.session.cwd.display().to_string(),
        });
        notify_session_ready(&app.notification_service, agent);
        note_peek_page_flip(app, agent_id, drain.page_flip_entry);
        return effects;
    }
    vec![]
}
pub(in crate::app::dispatch) fn handle_worktree_session_created(
    app: &mut AppView,
    agent_id: AgentId,
    session_id: acp::SessionId,
    worktree_path: std::path::PathBuf,
    session_cwd: std::path::PathBuf,
    new_models: Option<acp::SessionModelState>,
) -> Vec<Effect> {
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        agent.session.finish_command();
        agent.mark_turn_finished();
        let session_id_clone = session_id.clone();
        agent.bind_session_id(session_id);
        agent.session.cwd = session_cwd.clone();
        agent.session.is_worktree = true;
        if let Some(m) = new_models {
            app.models = Some(m).into();
            agent.session.models = app.models.clone();
        }
        agent.prompt.file_search.retarget(&session_cwd);
        agent.scrollback.push_block(RenderBlock::system(format!(
            "Worktree ready: {}",
            worktree_path.display()
        )));
        let deferred = apply_deferred_model_switch(agent, app.cli_effort_token.as_deref());
        let deferred_mode = agent.deferred_session_mode.take();
        let cwd = agent.session.cwd.clone();
        if deferred.is_some() {
            agent.session.model_switch_pending = true;
        }
        let mut drain = if app.reconnect_pending {
            QueueDrain {
                effects: vec![],
                page_flip_entry: None,
            }
        } else {
            maybe_drain_queue(agent)
        };
        let mut effects = std::mem::take(&mut drain.effects);
        agent.session.prompt_history_loading = true;
        effects.push(Effect::FetchPromptHistory {
            agent_id,
            cwd: cwd.clone(),
            session_id: session_id_clone.to_string(),
        });
        effects.push(Effect::FetchSessionAgentName {
            agent_id,
            session_id: session_id_clone.clone(),
        });
        effects.push(Effect::RefreshAvailableCommands { agent_id, cwd });
        effects.push(Effect::CheckMarketplaceUpdates {
            agent_id,
            session_id: session_id_clone.clone(),
        });
        if app.plugin_cta_enabled {
            effects.push(Effect::FetchPluginCtaCatalog {
                agent_id,
                session_id: session_id_clone.clone(),
            });
        }
        effects.push(Effect::FetchBilling {
            agent_id,
            silent: true,
        });
        if let Some((model_id, effort)) = deferred {
            effects.push(Effect::SwitchModel {
                agent_id,
                session_id: session_id_clone.clone(),
                model_id,
                effort,
                prev_model_id: None,
            });
        }
        if let Some(mode) = deferred_mode {
            effects.push(Effect::SetSessionMode {
                session_id: session_id_clone.clone(),
                mode_id: acp::SessionModeId::new(mode.as_id()),
            });
        }
        if std::mem::take(&mut agent.pending_extensions_fetch) && agent.extensions_modal.is_some() {
            effects.extend(extensions_modal_tab_fetches(
                agent_id,
                session_id_clone.clone(),
            ));
        }
        effects.push(Effect::RegisterActiveSession {
            session_id: session_id_clone,
            cwd: agent.session.cwd.display().to_string(),
        });
        notify_session_ready(&app.notification_service, agent);
        note_peek_page_flip(app, agent_id, drain.page_flip_entry);
        return effects;
    }
    vec![]
}
pub(in crate::app::dispatch) fn handle_worktree_session_failed(
    app: &mut AppView,
    agent_id: AgentId,
    error: String,
) -> Vec<Effect> {
    tracing::error!(
        agent = ? agent_id, error = % error, "Worktree session creation failed"
    );
    let is_orphan_zombie = app
        .agents
        .get(&agent_id)
        .is_some_and(|a| a.session.session_id.is_none() && a.session.forked_from.is_none());
    if is_orphan_zombie {
        let fallback = app.agents.keys().copied().find(|id| *id != agent_id);
        remove_agent_and_cleanup(app, agent_id);
        if let Some(target) = fallback {
            switch_to_agent(app, target, SwitchCause::Picker);
        } else {
            show_welcome(app);
            app.welcome_prompt_focused = true;
            app.session_picker_entries = None;
            app.session_picker_loading = false;
            app.session_picker_state.selected = 0;
            app.session_picker_content_results = None;
            app.session_picker_content_loading = false;
        }
        let msg = format!("Cannot create worktree: {error}");
        if !app.startup_warnings.iter().any(|w| w.message == msg) {
            app.startup_warnings.push(crate::startup::StartupWarning {
                severity: crate::startup::WarningSeverity::Warning,
                message: msg,
                action: None,
            });
        }
    } else if let Some(agent) = app.agents.get_mut(&agent_id) {
        agent.pending_extensions_fetch = false;
        agent.session.prompt_history_loading = false;
        agent.session.finish_command();
        let elapsed = agent.turn_elapsed();
        agent.mark_turn_finished();
        agent.pending_first_prompt = None;
        agent.pending_fork_banner = None;
        agent
            .scrollback
            .push_block(RenderBlock::session_event(SessionEvent::TurnFailed {
                error,
                elapsed,
            }));
    }
    vec![]
}
pub(in crate::app::dispatch) fn handle_switch_model_complete(
    app: &mut AppView,
    agent_id: AgentId,
    model_id: acp::ModelId,
    effort: Option<ReasoningEffort>,
    result: Result<(), SwitchModelError>,
    prev_model_id: Option<acp::ModelId>,
) -> Vec<Effect> {
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        agent.session.model_switch_pending = false;
        let mut effects = match result {
            Ok(()) => {
                agent.session.user_model_preference = Some(model_id.clone());
                let display_name = agent
                    .session
                    .models
                    .available
                    .get(&model_id)
                    .map(|info| info.name.clone())
                    .unwrap_or_else(|| model_id.0.to_string());
                let prev_model = agent.session.models.current.clone();
                let prev_effort = agent.session.models.reasoning_effort;
                agent.session.models.set_current(model_id.clone(), effort);
                let resolved_effort = agent.session.models.reasoning_effort;
                let unchanged =
                    prev_model.as_ref() == Some(&model_id) && prev_effort == resolved_effort;
                if !unchanged {
                    let msg = if let Some(eff) = resolved_effort {
                        format!("Switched to {display_name} ({eff} effort)")
                    } else {
                        format!("Switched to {display_name}")
                    };
                    agent.scrollback.push_block(RenderBlock::system(msg));
                }
                if unchanged {
                    vec![]
                } else {
                    vec![Effect::PersistPreferredModel {
                        model_id: model_id.clone(),
                        reasoning_effort: resolved_effort,
                    }]
                }
            }
            Err(SwitchModelError::IncompatibleAgent { .. }) => {
                if let Some(ref prev) = prev_model_id {
                    agent.session.models.set_current(prev.clone(), None);
                }
                agent.active_modal = None;
                let display_name = agent.session.models.display_name_for(&model_id);
                return open_agent_type_mismatch_question(app, model_id, effort, &display_name);
            }
            Err(SwitchModelError::Other(msg)) => {
                agent
                    .scrollback
                    .push_block(RenderBlock::system(format!("Couldn't switch model: {msg}")));
                vec![]
            }
        };
        let drain = maybe_drain_queue(agent);
        effects.extend(drain.effects);
        note_peek_page_flip(app, agent_id, drain.page_flip_entry);
        effects
    } else {
        vec![]
    }
}
pub(in crate::app::dispatch) fn dispatch_agent_type_mismatch_answered(
    app: &mut AppView,
    start_new: bool,
    model_id: acp::ModelId,
    effort: Option<ReasoningEffort>,
) -> Vec<Effect> {
    if start_new {
        let effects = dispatch_new_session_inner(app, Some(model_id.clone()));
        if let ActiveView::Agent(new_aid) = app.active_view
            && let Some(agent) = app.agents.get_mut(&new_aid)
        {
            agent.session.models.set_current(model_id.clone(), effort);
            if effort.is_some() {
                agent.session.deferred_model_switch = Some((model_id, effort));
            }
        }
        effects
    } else {
        vec![]
    }
}
