//! Tests for session create, exit, trust, startup actions, worktree creation, and cloud lifecycle.
use super::*;
/// Simulate a release-stamped build so folder-trust is active (a local/dev
/// build auto-trusts and persists nothing). Mirrors this module's raw env idiom.
fn simulate_release_build() {
    unsafe { std::env::set_var(xai_grok_version::TEST_VERSION_ENV, "0.0.0-sim") };
}
#[test]
fn voice_on_welcome_creates_session_and_records() {
    if !xai_grok_voice::AUDIO_SUPPORTED {
        return;
    }
    let mut app = test_app();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    app.voice_mode_enabled = true;
    app.voice_cmd_tx = Some(tx);
    assert!(app.agents.is_empty());
    dispatch(Action::EnableVoiceMode, &mut app);
    let ActiveView::Agent(id) = app.active_view else {
        panic!("voice on welcome must create and switch to a session");
    };
    assert!(app.voice_listening(), "capture starts into the new session");
    assert_eq!(app.voice_recording_target(), Some(VoiceTarget::Agent(id)));
    assert!(matches!(
        rx.try_recv(),
        Ok(xai_grok_voice::VoiceCommand::PttPress)
    ));
}
#[test]
fn voice_final_routes_to_recording_session_not_active_view() {
    let mut app = test_app_with_agent();
    let rec = AgentId(0);
    let other = AgentId(1);
    let session = make_test_agent_session(&app, other, "second");
    app.agents
        .insert(other, AgentView::new(session, ScrollbackState::new()));
    app.active_view = ActiveView::Agent(other);
    app.voice_state = VoiceState::Stopping {
        target: VoiceTarget::Agent(rec),
        interim: None,
    };
    crate::voice::handle_voice_event(
        &mut app,
        xai_grok_voice::VoiceEvent::UtteranceFinal {
            text: "hello".into(),
        },
    );
    assert_eq!(app.agents.get(&rec).unwrap().prompt.text(), "hello");
    assert_eq!(app.agents.get(&other).unwrap().prompt.text(), "");
}
#[test]
fn voice_final_dropped_after_recording_session_cleared() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.voice_state = VoiceState::Idle;
    crate::voice::handle_voice_event(
        &mut app,
        xai_grok_voice::VoiceEvent::UtteranceFinal {
            text: "late".into(),
        },
    );
    assert_eq!(app.agents.get(&id).unwrap().prompt.text(), "");
}
#[test]
fn voice_auto_stops_when_leaving_recording_session() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    app.voice_cmd_tx = Some(tx);
    app.voice_state = VoiceState::Recording {
        hold: false,
        target: VoiceTarget::Agent(id),
        interim: None,
    };
    app.active_view = ActiveView::Agent(id);
    app.enforce_voice_session_bound();
    assert!(app.voice_listening());
    assert!(rx.try_recv().is_err());
    app.active_view = ActiveView::AgentDashboard;
    app.enforce_voice_session_bound();
    assert!(!app.voice_listening(), "must stop when leaving the session");
    assert!(app.voice_interim().is_none());
    assert!(
        app.voice_recording_target().is_none(),
        "target dropped on leave"
    );
    assert!(matches!(
        rx.try_recv(),
        Ok(xai_grok_voice::VoiceCommand::PttRelease)
    ));
}
#[test]
fn chip_submit_without_session_keeps_chips_and_does_not_send() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.session_id = None;
        agent.apply_follow_ups("resp-1".into(), vec!["Summarize".into()]);
        assert!(agent.follow_ups.is_some(), "precondition: chips shown");
    }
    let effects = dispatch(Action::SubmitFollowUp("Summarize".into()), &mut app);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::SendPrompt { .. })),
        "an unbound-session submit must emit no SendPrompt, got {effects:?}"
    );
    assert!(
        app.agents[&id].follow_ups.is_some(),
        "an unbound-session submit must NOT clear the chips"
    );
}
#[test]
fn send_prompt_without_session_queues_but_no_effect() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().session.session_id = None;
    let effects = dispatch(Action::SendPrompt("hello".into()), &mut app);
    assert!(effects.is_empty());
    assert_eq!(app.agents[&id].session.queue_len(), 1);
    assert_eq!(app.agents[&id].session.pending_prompts[0].text, "hello");
}
#[test]
fn session_created_sets_session_id() {
    let mut app = test_app_with_agent();
    app.plugin_cta_enabled = true;
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().session.session_id = None;
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionCreated {
            agent_id: id,
            session_id: "new-session-123".into(),
            models: None,
        }),
        &mut app,
    );
    assert_eq!(effects.len(), 7);
    assert!(matches!(
        &effects[0],
        Effect::FetchPromptHistory { session_id, .. } if session_id == "new-session-123"
    ));
    assert!(matches!(&effects[1], Effect::FetchSessionAgentName { .. }));
    assert!(matches!(
        &effects[2],
        Effect::RefreshAvailableCommands { .. }
    ));
    assert!(matches!(
        &effects[3],
        Effect::CheckMarketplaceUpdates { .. }
    ));
    assert!(matches!(&effects[4], Effect::FetchPluginCtaCatalog { .. }));
    assert!(matches!(
        &effects[5],
        Effect::FetchBilling { silent: true, .. }
    ));
    assert!(matches!(&effects[6], Effect::RegisterActiveSession { .. }));
    assert_eq!(
        app.agents[&id]
            .session
            .session_id
            .as_ref()
            .map(|s| s.0.as_ref()),
        Some("new-session-123")
    );
}
#[test]
fn session_created_omits_cta_catalog_when_disabled() {
    let mut app = test_app_with_agent();
    assert!(!app.plugin_cta_enabled);
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().session.session_id = None;
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionCreated {
            agent_id: id,
            session_id: "new-session-123".into(),
            models: None,
        }),
        &mut app,
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::FetchPluginCtaCatalog { .. }))
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::CheckMarketplaceUpdates { .. }))
    );
}
/// All System-block texts in an agent's scrollback, in order.
fn all_system_texts(app: &AppView, id: AgentId) -> Vec<String> {
    let sb = &app.agents[&id].scrollback;
    (0..sb.len())
        .filter_map(|i| match &sb.get(i).expect("index in range").block {
            RenderBlock::System(sys) => Some(sys.text.clone()),
            _ => None,
        })
        .collect()
}
/// In minimal mode the `/new` session banner must advertise `/resume`, not
/// `/dashboard` — the dashboard command is refused there, while the
/// `/resume` session picker works. Deterministic regardless of the
/// dashboard feature flag: the minimal branch of
/// `session_switch_hint_command` never consults it.
#[test]
fn session_created_banner_advertises_resume_in_minimal_mode() {
    let mut app = test_app_with_agent();
    app.screen_mode = crate::app::ScreenMode::Minimal;
    let id = AgentId(1);
    let mut session = make_test_agent_session(&app, id, "unused");
    session.session_id = None;
    session.created_via_new = true;
    app.agents
        .insert(id, AgentView::new(session, ScrollbackState::new()));
    dispatch(
        Action::TaskComplete(TaskResult::SessionCreated {
            agent_id: id,
            session_id: "new-session-123".into(),
            models: None,
        }),
        &mut app,
    );
    let texts = all_system_texts(&app, id);
    let banner = texts
        .iter()
        .find(|t| t.contains("switch between sessions"))
        .unwrap_or_else(|| panic!("expected a session-switch banner, got: {texts:?}"));
    assert!(
        banner.contains("Session new-session-123 \u{2014} use /resume to switch between sessions"),
        "minimal mode must advertise /resume: {banner}"
    );
    assert!(
        !texts.iter().any(|t| t.contains("/dashboard")),
        "minimal mode must NOT advertise /dashboard: {texts:?}"
    );
}
#[test]
fn global_cancel_subagents_pref_skips_panel_without_session_override() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent
            .subagent_sessions
            .insert("child-1".into(), make_test_subagent("child-1", "sa-1"));
    }
    app.current_ui.cancel_subagents_on_turn_cancel = Some("always_continue".into());
    let effects = dispatch(Action::CancelTurn, &mut app);
    assert!(app.agents[&id].cancel_turn_view.is_none());
    assert!(matches!(
        effects.as_slice(),
        [Effect::CancelTurn {
            cancel_subagents: false,
            ..
        }]
    ));
}
#[test]
fn new_worktree_session_creates_agent_and_returns_effect() {
    let mut app = test_app_git();
    let effects = dispatch(
        Action::NewWorktreeSession {
            load_session_id: None,
            label: None,
            git_ref: None,
        },
        &mut app,
    );
    assert_eq!(effects.len(), 1);
    assert!(matches!(effects[0], Effect::CreateWorktreeSession { .. }));
    assert!(matches!(app.active_view, ActiveView::Agent(AgentId(0))));
    assert!(app.agents.contains_key(&AgentId(0)));
    assert_eq!(app.agents[&AgentId(0)].scrollback.len(), 0);
    assert!(matches!(
        app.agents[&AgentId(0)].session.state,
        AgentState::CommandRunning { .. }
    ));
}
#[test]
fn worktree_session_created_sets_session_and_cwd() {
    let mut app = test_app_git();
    dispatch(
        Action::NewWorktreeSession {
            load_session_id: None,
            label: None,
            git_ref: None,
        },
        &mut app,
    );
    let id = AgentId(0);
    let worktree_path = PathBuf::from("/tmp/grok-worktrees/pager-123");
    let session_cwd = worktree_path.clone();
    let effects = dispatch(
        Action::TaskComplete(TaskResult::WorktreeSessionCreated {
            agent_id: id,
            session_id: acp::SessionId::new("wt-session-1"),
            worktree_path: worktree_path.clone(),
            session_cwd: session_cwd.clone(),
            models: None,
        }),
        &mut app,
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::FetchPromptHistory { .. }))
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::FetchSessionAgentName { .. }))
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::FetchBilling { silent: true, .. }))
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::RegisterActiveSession { .. }))
    );
    assert_eq!(
        app.agents[&id]
            .session
            .session_id
            .as_ref()
            .map(|s| s.0.as_ref()),
        Some("wt-session-1")
    );
    assert_eq!(app.agents[&id].session.cwd, session_cwd);
    assert_eq!(app.agents[&id].scrollback.len(), 1);
    assert!(app.agents[&id].session.state.is_idle());
}
#[test]
fn worktree_session_preserves_subdirectory_offset() {
    let mut app = test_app();
    app.cwd = PathBuf::from("/repo/crates/codegen/xai-grok-pager");
    app.cwd_has_git_ancestor = true;
    dispatch(
        Action::NewWorktreeSession {
            load_session_id: None,
            label: None,
            git_ref: None,
        },
        &mut app,
    );
    let id = AgentId(0);
    let worktree_root = PathBuf::from("/home/user/.grok/worktrees/repo/pager-123");
    let session_cwd = worktree_root.join("crates/codegen/xai-grok-pager");
    let effects = dispatch(
        Action::TaskComplete(TaskResult::WorktreeSessionCreated {
            agent_id: id,
            session_id: acp::SessionId::new("wt-nested-1"),
            worktree_path: worktree_root.clone(),
            session_cwd: session_cwd.clone(),
            models: None,
        }),
        &mut app,
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::FetchPromptHistory { .. }))
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::RegisterActiveSession { .. }))
    );
    assert_eq!(app.agents[&id].session.cwd, session_cwd);
    assert_ne!(app.agents[&id].session.cwd, worktree_root);
}
#[test]
fn worktree_session_failed_without_session_returns_to_welcome() {
    let mut app = test_app_git();
    dispatch(
        Action::NewWorktreeSession {
            load_session_id: None,
            label: None,
            git_ref: None,
        },
        &mut app,
    );
    let id = AgentId(0);
    assert!(app.agents.contains_key(&id));
    assert!(matches!(app.active_view, ActiveView::Agent(AgentId(0))));
    let error_msg = "worktree creation failed: not a git repo";
    let effects = dispatch(
        Action::TaskComplete(TaskResult::WorktreeSessionFailed {
            agent_id: id,
            error: error_msg.into(),
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    assert!(!app.agents.contains_key(&id));
    assert!(matches!(app.active_view, ActiveView::Welcome));
    assert!(
        app.startup_warnings
            .iter()
            .any(|w| w.message.contains(error_msg)),
        "expected warning containing the error message"
    );
}
#[test]
fn worktree_session_failed_with_fork_parent_keeps_agent() {
    let mut app = test_app_git();
    dispatch(
        Action::NewWorktreeSession {
            load_session_id: None,
            label: None,
            git_ref: None,
        },
        &mut app,
    );
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().session.forked_from = Some(AgentId(99));
    let error_msg = "worktree creation failed: permission denied";
    let effects = dispatch(
        Action::TaskComplete(TaskResult::WorktreeSessionFailed {
            agent_id: id,
            error: error_msg.into(),
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    assert!(app.agents.contains_key(&id));
    assert!(app.agents[&id].session.state.is_idle());
    let entry = app.agents[&id].scrollback.entry(0).unwrap();
    match &entry.block {
        RenderBlock::SessionEvent(block) => match &block.event {
            SessionEvent::TurnFailed { error, .. } => {
                assert_eq!(error, error_msg);
            }
            other => panic!("expected TurnFailed, got {other:?}"),
        },
        other => panic!("expected SessionEvent block, got {other:?}"),
    }
}
#[test]
fn new_worktree_session_rejects_non_git_cwd() {
    let mut app = test_app();
    let effects = dispatch(
        Action::NewWorktreeSession {
            load_session_id: None,
            label: None,
            git_ref: None,
        },
        &mut app,
    );
    assert!(effects.is_empty());
    assert!(app.agents.is_empty());
    assert!(matches!(app.active_view, ActiveView::Welcome));
    let warning = app
        .startup_warnings
        .iter()
        .find(|warning| warning.message.contains("Not inside a git repository"))
        .expect("expected git-repo warning");
    assert_eq!(warning.severity, crate::startup::WarningSeverity::Warning);
    assert!(warning.action.is_none());
}
#[test]
fn worktree_session_created_drains_queued_prompts() {
    let mut app = test_app_git();
    dispatch(
        Action::NewWorktreeSession {
            load_session_id: None,
            label: None,
            git_ref: None,
        },
        &mut app,
    );
    let id = AgentId(0);
    let effects = dispatch(Action::SendPrompt("hello".into()), &mut app);
    assert!(effects.is_empty(), "no session_id yet, can't drain");
    assert_eq!(app.agents[&id].session.queue_len(), 1);
    let worktree_path = PathBuf::from("/tmp/grok-worktrees/pager-abc");
    let effects = dispatch(
        Action::TaskComplete(TaskResult::WorktreeSessionCreated {
            agent_id: id,
            session_id: acp::SessionId::new("wt-drain-1"),
            worktree_path,
            session_cwd: PathBuf::from("/tmp/grok-worktrees/pager-abc"),
            models: None,
        }),
        &mut app,
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::SendPrompt { text, .. } if text == "hello"))
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::FetchPromptHistory { .. }))
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::RegisterActiveSession { .. }))
    );
    assert_eq!(app.agents[&id].session.queue_len(), 0);
}
#[test]
fn session_created_drains_queued_prompts() {
    let mut app = test_app();
    dispatch(Action::NewSession, &mut app);
    let id = AgentId(0);
    let effects = dispatch(Action::SendPrompt("queued msg".into()), &mut app);
    assert!(effects.is_empty());
    assert_eq!(app.agents[&id].session.queue_len(), 1);
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionCreated {
            agent_id: id,
            session_id: acp::SessionId::new("sess-drain-1"),
            models: None,
        }),
        &mut app,
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::SendPrompt { text, .. } if text == "queued msg"))
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::FetchPromptHistory { .. }))
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::RegisterActiveSession { .. }))
    );
    assert_eq!(app.agents[&id].session.queue_len(), 0);
}
#[test]
fn session_created_with_flag_emits_five_fetches_and_clears_flag() {
    use crate::views::extensions_modal::{ExtensionsModalState, ExtensionsTab};
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let a = app.agents.get_mut(&id).unwrap();
        a.session.session_id = None;
        a.pending_extensions_fetch = true;
        a.extensions_modal = Some(ExtensionsModalState::new(ExtensionsTab::Hooks));
    }
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionCreated {
            agent_id: id,
            session_id: acp::SessionId::new("s"),
            models: None,
        }),
        &mut app,
    );
    assert_eq!(count_extension_fetches(&effects), 5);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::FetchMcpsList { cache: true, .. }))
    );
    assert!(!app.agents[&id].pending_extensions_fetch);
}
#[test]
fn session_created_without_flag_emits_no_extension_fetches() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().session.session_id = None;
    assert!(!app.agents[&id].pending_extensions_fetch);
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionCreated {
            agent_id: id,
            session_id: acp::SessionId::new("s"),
            models: None,
        }),
        &mut app,
    );
    assert_eq!(count_extension_fetches(&effects), 0);
}
#[test]
fn session_failed_keeps_agent_clears_loading_and_toasts() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let a = app.agents.get_mut(&id).unwrap();
        a.session.session_id = Some(acp::SessionId::new("existing"));
        a.pending_extensions_fetch = true;
        a.mcp_init_progress = Some(crate::app::agent_view::McpInitProgress {
            total: 0,
            connected: 0,
            started_at: std::time::Instant::now(),
        });
    }
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionFailed {
            agent_id: id,
            error: "No space left on device".to_string(),
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    let agent = &app.agents[&id];
    assert!(!agent.pending_extensions_fetch);
    assert!(agent.mcp_init_progress.is_none());
    assert_eq!(
        agent.toast.as_ref().map(|(m, _)| m.as_str()),
        Some("Session creation failed: No space left on device"),
    );
}
#[test]
fn session_failed_orphan_returns_to_welcome_with_warning() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let a = app.agents.get_mut(&id).unwrap();
        a.session.session_id = None;
        a.session.forked_from = None;
        a.mcp_init_progress = Some(crate::app::agent_view::McpInitProgress {
            total: 0,
            connected: 0,
            started_at: std::time::Instant::now(),
        });
    }
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionFailed {
            agent_id: id,
            error: "No space left on device".to_string(),
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    assert!(!app.agents.contains_key(&id));
    assert!(matches!(app.active_view, ActiveView::Welcome));
    assert!(
        app.startup_warnings
            .iter()
            .any(|w| { w.message == "Session creation failed: No space left on device" })
    );
}
#[test]
fn session_failed_orphan_with_fallback_toasts() {
    let mut app = test_app_with_agent();
    let keep_id = AgentId(0);
    let fail_id = AgentId(1);
    let mut session = make_test_agent_session(&app, fail_id, "unused");
    session.session_id = None;
    app.agents
        .insert(fail_id, AgentView::new(session, ScrollbackState::new()));
    app.active_view = ActiveView::Agent(fail_id);
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionFailed {
            agent_id: fail_id,
            error: "No space left on device".to_string(),
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    assert!(!app.agents.contains_key(&fail_id));
    assert!(matches!(app.active_view, ActiveView::Agent(id) if id == keep_id));
    assert_eq!(
        app.agents[&keep_id].toast.as_ref().map(|(m, _)| m.as_str()),
        Some("Session creation failed: No space left on device"),
    );
}
#[test]
fn session_failed_orphan_does_not_steal_other_active_agent() {
    let mut app = test_app_with_agent();
    let keep_id = AgentId(0);
    let fail_id = AgentId(1);
    let mut session = make_test_agent_session(&app, fail_id, "unused");
    session.session_id = None;
    app.agents
        .insert(fail_id, AgentView::new(session, ScrollbackState::new()));
    app.active_view = ActiveView::Agent(keep_id);
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionFailed {
            agent_id: fail_id,
            error: "No space left on device".to_string(),
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    assert!(!app.agents.contains_key(&fail_id));
    assert!(matches!(app.active_view, ActiveView::Agent(id) if id == keep_id));
    assert_eq!(
        app.agents[&keep_id].toast.as_ref().map(|(m, _)| m.as_str()),
        Some("Session creation failed: No space left on device"),
    );
}
#[test]
fn session_failed_orphan_on_welcome_with_survivor_uses_startup_warning() {
    let mut app = test_app_with_agent();
    let keep_id = AgentId(0);
    let fail_id = AgentId(1);
    let mut session = make_test_agent_session(&app, fail_id, "unused");
    session.session_id = None;
    app.agents
        .insert(fail_id, AgentView::new(session, ScrollbackState::new()));
    app.active_view = ActiveView::Welcome;
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionFailed {
            agent_id: fail_id,
            error: "No space left on device".to_string(),
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    assert!(!app.agents.contains_key(&fail_id));
    assert!(app.agents.contains_key(&keep_id));
    assert!(matches!(app.active_view, ActiveView::Welcome));
    assert!(
        app.startup_warnings
            .iter()
            .any(|w| w.message == "Session creation failed: No space left on device"),
        "Welcome + survivor must record a startup warning; got {:?}",
        app.startup_warnings
            .iter()
            .map(|w| w.message.as_str())
            .collect::<Vec<_>>(),
    );
    assert!(
        app.agents[&keep_id].toast.is_none(),
        "must not force-switch to survivor just to toast"
    );
}
#[test]
fn switch_model_without_session_does_nothing() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().session.session_id = None;
    let model_id = acp::ModelId::new(std::sync::Arc::from("grok-4.5"));
    let effects = dispatch(
        Action::SwitchModel {
            model_id,
            effort: None,
        },
        &mut app,
    );
    assert!(effects.is_empty());
    assert!(!app.agents[&id].session.model_switch_pending);
}
#[test]
fn agent_type_mismatch_start_new_creates_session_with_model_id() {
    let mut app = test_app_with_agent();
    let model_id = acp::ModelId::new(std::sync::Arc::from("cursor-model"));
    let effects = dispatch(
        Action::AgentTypeMismatchAnswered {
            start_new: true,
            model_id: model_id.clone(),
            effort: None,
        },
        &mut app,
    );
    let create = effects
        .iter()
        .find(|e| matches!(e, Effect::CreateSession { .. }));
    assert!(create.is_some(), "expected CreateSession effect");
    match create.unwrap() {
        Effect::CreateSession { model_id: mid, .. } => {
            assert_eq!(
                mid.as_ref(),
                Some(&model_id),
                "CreateSession must carry the target model_id",
            );
        }
        _ => unreachable!(),
    }
    if let ActiveView::Agent(new_aid) = app.active_view {
        let agent = &app.agents[&new_aid];
        assert!(
            agent.session.deferred_model_switch.is_none(),
            "no-effort mismatch must not set deferred_model_switch",
        );
    } else {
        panic!("expected active view to be an Agent");
    }
}
#[test]
fn new_session_seeds_mcp_init_progress() {
    let mut app = test_app();
    dispatch(Action::NewSession, &mut app);
    let id = AgentId(0);
    let progress = app.agents[&id].mcp_init_progress.as_ref();
    assert!(
        progress.is_some(),
        "new session must seed mcp_init_progress",
    );
    let p = progress.unwrap();
    assert_eq!(
        p.total, 0,
        "seeded total must be 0 (unknown until shell reports)"
    );
    assert_eq!(p.connected, 0, "seeded connected must be 0");
}
#[test]
fn new_session_without_model_switch_has_no_model_id() {
    let mut app = test_app();
    let effects = dispatch(Action::NewSession, &mut app);
    let create = effects
        .iter()
        .find(|e| matches!(e, Effect::CreateSession { .. }));
    assert!(create.is_some());
    match create.unwrap() {
        Effect::CreateSession { model_id, .. } => {
            assert!(model_id.is_none(), "plain /new must not carry a model_id",);
        }
        _ => unreachable!(),
    }
}
#[test]
fn new_session_seeds_available_commands_from_bootstrap() {
    let mut app = test_app();
    app.bootstrap_acp_commands = vec![acp::AvailableCommand::new(
        "flush".to_string(),
        "Flush memory".to_string(),
    )];
    dispatch(Action::NewSession, &mut app);
    let id = AgentId(0);
    assert_eq!(app.agents[&id].session.available_commands.len(), 1);
    assert_eq!(app.agents[&id].session.available_commands[0].name, "flush");
    assert_eq!(app.agents[&id].session.available_commands_generation, 1);
}
#[test]
fn new_session_empty_bootstrap_starts_at_generation_1() {
    let mut app = test_app();
    app.bootstrap_acp_commands = Vec::new();
    dispatch(Action::NewSession, &mut app);
    let id = AgentId(0);
    assert!(app.agents[&id].session.available_commands.is_empty());
    assert_eq!(app.agents[&id].session.available_commands_generation, 1);
}
#[test]
fn worktree_session_seeds_available_commands_from_bootstrap() {
    let mut app = test_app_git();
    app.bootstrap_acp_commands = vec![acp::AvailableCommand::new(
        "plugins".to_string(),
        "List plugins".to_string(),
    )];
    dispatch(
        Action::NewWorktreeSession {
            load_session_id: None,
            label: None,
            git_ref: None,
        },
        &mut app,
    );
    let id = AgentId(0);
    assert_eq!(app.agents[&id].session.available_commands.len(), 1);
    assert_eq!(
        app.agents[&id].session.available_commands[0].name,
        "plugins"
    );
    assert_eq!(app.agents[&id].session.available_commands_generation, 1);
}
#[test]
fn exit_session_unregisters_active_session() {
    let mut app = test_app_with_agent();
    let effects = dispatch(Action::ExitSession, &mut app);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::UnregisterActiveSession { .. })),
        "ExitSession must emit UnregisterActiveSession, got: {effects:?}"
    );
    assert!(matches!(app.active_view, ActiveView::Welcome));
}
#[test]
fn slash_new_dispatches_new_session() {
    let mut app = test_app_with_agent();
    let effects = dispatch(Action::SendPrompt("/new".into()), &mut app);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::CreateSession { .. }))
    );
}
#[test]
fn new_session_falls_back_to_app_cwd_on_welcome_screen() {
    let mut app = test_app();
    assert!(matches!(app.active_view, ActiveView::Welcome));
    let effects = dispatch(Action::NewSession, &mut app);
    let create = effects
        .iter()
        .find(|e| matches!(e, Effect::CreateSession { .. }));
    assert!(create.is_some(), "expected CreateSession effect");
    match create.unwrap() {
        Effect::CreateSession { cwd, .. } => assert_eq!(cwd, &PathBuf::from("/tmp")),
        _ => unreachable!(),
    }
    let new_id = AgentId(0);
    assert!(!app.agents[&new_id].session.is_worktree);
}
#[test]
fn new_session_starts_with_prompt_focused() {
    let mut app = test_app();
    assert!(matches!(app.active_view, ActiveView::Welcome));
    dispatch(Action::NewSession, &mut app);
    let id = AgentId(0);
    assert_eq!(app.agents[&id].active_pane, ActivePane::Prompt);
}
#[test]
fn switch_model_deferred_when_no_session_id() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let model_id = acp::ModelId::new(std::sync::Arc::from("grok-4.5"));
    app.agents.get_mut(&id).unwrap().session.session_id = None;
    let effects = dispatch(
        Action::SwitchModel {
            model_id: model_id.clone(),
            effort: None,
        },
        &mut app,
    );
    assert!(effects.is_empty());
    assert_eq!(
        app.agents[&id].session.deferred_model_switch,
        Some((model_id, None))
    );
    assert!(!app.agents[&id].session.model_switch_pending);
}
#[test]
fn deferred_model_switch_applied_on_session_created() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let model_id = acp::ModelId::new(std::sync::Arc::from("grok-4.5"));
    let session_id: acp::SessionId = "new-session".into();
    app.agents.get_mut(&id).unwrap().session.session_id = None;
    app.agents
        .get_mut(&id)
        .unwrap()
        .session
        .deferred_model_switch = Some((model_id.clone(), None));
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionCreated {
            agent_id: id,
            session_id: session_id.clone(),
            models: None,
        }),
        &mut app,
    );
    assert!(app.agents[&id].session.deferred_model_switch.is_none());
    assert!(app.agents[&id].session.model_switch_pending);
    assert!(effects.iter().any(|e| matches!(
        e,
        Effect::SwitchModel {
            agent_id: a_id,
            session_id: s_id,
            model_id: m_id,
            .. } if *a_id == id && *s_id == session_id && *m_id == model_id
    )));
}
#[test]
fn deferred_model_switch_applied_on_worktree_session_created() {
    let mut app = test_app_git();
    let model_id = acp::ModelId::new(std::sync::Arc::from("grok-4.5"));
    dispatch(
        Action::NewWorktreeSession {
            load_session_id: None,
            label: None,
            git_ref: None,
        },
        &mut app,
    );
    let id = AgentId(0);
    app.agents
        .get_mut(&id)
        .unwrap()
        .session
        .deferred_model_switch = Some((model_id.clone(), None));
    let session_id: acp::SessionId = "wt-session".into();
    let effects = dispatch(
        Action::TaskComplete(TaskResult::WorktreeSessionCreated {
            agent_id: id,
            session_id: session_id.clone(),
            worktree_path: PathBuf::from("/tmp/worktree"),
            session_cwd: PathBuf::from("/tmp/worktree"),
            models: None,
        }),
        &mut app,
    );
    assert!(app.agents[&id].session.deferred_model_switch.is_none());
    assert!(app.agents[&id].session.model_switch_pending);
    assert!(effects.iter().any(|e| matches!(
        e,
        Effect::SwitchModel {
            agent_id: a_id,
            session_id: s_id,
            model_id: m_id,
            .. } if *a_id == id && *s_id == session_id && *m_id == model_id
    )));
}
/// The session-startup gate requires BOTH auth AND trust resolved. Trust is
/// gated AFTER auth, so either one pending defers session creation.
#[test]
fn session_startup_allowed_requires_auth_and_trust() {
    let mut app = test_app();
    assert!(app.session_startup_allowed());
    app.trust_state = TrustState::Pending {
        workspace: PathBuf::from("/x"),
    };
    assert!(
        !app.session_startup_allowed(),
        "pending trust must block session startup",
    );
    app.trust_state = TrustState::Done;
    app.auth_state = AuthState::Pending { error: None };
    assert!(
        !app.session_startup_allowed(),
        "pending auth must block session startup",
    );
    app.trust_state = TrustState::Pending {
        workspace: PathBuf::from("/x"),
    };
    assert!(
        !app.session_startup_allowed(),
        "both pending must block session startup",
    );
}
/// Accepting the trust question (its `finish_trust` tail) resolves trust and
/// replays the deferred startup when auth is already done. (Declining quits
/// instead -- see `welcome_trust_decline_keys_quit` in `app_view`.)
#[test]
fn finish_trust_resolves_and_replays_startup() {
    let mut app = test_app();
    app.trust_state = TrustState::Pending {
        workspace: PathBuf::from("/x"),
    };
    app.deferred_startup.session =
        Some(crate::app::session_startup::DeferredSessionStartup::Load {
            session_id: "deferred-session".into(),
            session_cwd: None,
            chat_kind: false,
        });
    let effects = finish_trust(&mut app);
    assert!(matches!(app.trust_state, TrustState::Done));
    assert!(
        app.deferred_startup.session.is_none(),
        "resolving trust must drain the deferred startup",
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::LoadSession { .. })),
        "deferred load must replay once trust resolves",
    );
}
/// Accepting the trust question persists the grant to the store and resolves
/// trust. GROK_HOME-isolated so the write hits a temp store, not the real one.
#[serial_test::serial(GROK_HOME)]
#[test]
fn trust_folder_grants_and_resolves() {
    use xai_grok_workspace::trust::{TrustStore, workspace_key};
    let home = tempfile::tempdir().expect("home tempdir");
    unsafe { std::env::set_var("GROK_HOME", home.path()) };
    simulate_release_build();
    let repo = tempfile::tempdir().expect("repo tempdir");
    let workspace = workspace_key(repo.path());
    let mut app = test_app();
    app.trust_state = TrustState::Pending {
        workspace: workspace.clone(),
    };
    let _ = dispatch(Action::TrustFolder, &mut app);
    assert!(matches!(app.trust_state, TrustState::Done));
    assert!(
        TrustStore::load().is_trusted(&workspace),
        "accepting must persist the trust grant for the workspace",
    );
}
/// When BOTH auth and trust are pending, `AuthComplete` must NOT replay the
/// deferred startup -- the trust question renders next, and its answer drains
/// it. Verifies the symmetric two-gate hand-off.
#[test]
fn auth_complete_defers_startup_until_trust_resolved() {
    let mut app = test_app();
    app.auth_state = AuthState::Authenticating {
        request_seq: 1,
        handle: None,
        auth_url: None,
        mode: AuthMode::Pending,
    };
    app.trust_state = TrustState::Pending {
        workspace: PathBuf::from("/x"),
    };
    app.deferred_startup.session =
        Some(crate::app::session_startup::DeferredSessionStartup::Load {
            session_id: "deferred-session".into(),
            session_cwd: None,
            chat_kind: false,
        });
    let effects = dispatch(
        Action::TaskComplete(TaskResult::AuthComplete {
            request_seq: 1,
            meta: None,
        }),
        &mut app,
    );
    assert!(matches!(app.auth_state, AuthState::Done));
    assert!(
        matches!(app.trust_state, TrustState::Pending { .. }),
        "trust stays pending after auth completes",
    );
    assert!(
        app.deferred_startup.session.is_some(),
        "startup must remain deferred while trust is pending",
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::LoadSession { .. })),
        "no session may load before trust is answered",
    );
    let effects = finish_trust(&mut app);
    assert!(app.deferred_startup.session.is_none());
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::LoadSession { .. })),
        "trust answer must replay the deferred startup",
    );
}
/// Symmetric to the above: trust answered FIRST while auth is still pending
/// exercises `finish_trust`'s `else` (deferred) branch -- it must NOT drain;
/// the later `AuthComplete` (the last gate) drains.
#[test]
fn trust_answered_first_defers_startup_until_auth_completes() {
    let mut app = test_app();
    app.auth_state = AuthState::Authenticating {
        request_seq: 1,
        handle: None,
        auth_url: None,
        mode: AuthMode::Pending,
    };
    app.trust_state = TrustState::Pending {
        workspace: PathBuf::from("/x"),
    };
    app.deferred_startup.session =
        Some(crate::app::session_startup::DeferredSessionStartup::Load {
            session_id: "deferred-session".into(),
            session_cwd: None,
            chat_kind: false,
        });
    let effects = finish_trust(&mut app);
    assert!(matches!(app.trust_state, TrustState::Done));
    assert!(
        app.deferred_startup.session.is_some(),
        "startup stays deferred while auth is still pending",
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::LoadSession { .. })),
        "no session may load before auth completes",
    );
    let effects = dispatch(
        Action::TaskComplete(TaskResult::AuthComplete {
            request_seq: 1,
            meta: None,
        }),
        &mut app,
    );
    assert!(matches!(app.auth_state, AuthState::Done));
    assert!(app.deferred_startup.session.is_none());
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::LoadSession { .. })),
        "auth completion replays the deferred startup",
    );
}
/// Regression / negative-space: a session-creating dispatch (e.g. the
/// global `Ctrl+N` `NewSession`, which bypasses the welcome interceptor) must
/// NOT create a session while `TrustState::Pending` -- the dispatch
/// chokepoint stashes it, and it replays exactly once when trust resolves.
#[test]
fn new_session_is_gated_while_trust_pending() {
    let mut app = test_app();
    app.trust_state = TrustState::Pending {
        workspace: PathBuf::from("/x"),
    };
    assert!(app.agents.is_empty());
    let effects = dispatch(Action::NewSession, &mut app);
    assert!(
        app.agents.is_empty(),
        "no session may be created while trust is Pending",
    );
    assert!(
        app.deferred_startup.new_session,
        "the new-session intent must be stashed for replay",
    );
    assert!(effects.is_empty(), "a gated NewSession produces no effects");
    let _ = finish_trust(&mut app);
    assert!(matches!(app.trust_state, TrustState::Done));
    assert!(
        !app.deferred_startup.new_session,
        "the stashed new-session intent is consumed on replay",
    );
    assert_eq!(
        app.agents.len(),
        1,
        "the deferred new session replays exactly once after trust resolves",
    );
}
/// Sticky `--chat` first create (NewSession) stamps chat_kind.
#[test]
fn chat_mode_new_session_creates_with_chat_kind() {
    let mut app = test_app();
    app.chat_mode = true;
    let effects = dispatch(Action::NewSession, &mut app);
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::CreateSession {
                chat_kind: true,
                ..
            }
        )),
        "expected chat CreateSession under --chat, got {effects:?}"
    );
}
/// Atomicity: when several startup intents coexist (e.g. CLI
/// `--resume` + an incidental `Ctrl+N` deferred during the trust question),
/// `drain_startup_actions` replays the highest-priority one and leaves NO
/// `startup_*` field set — no stale intent can fire on a later drain.
#[test]
fn drain_clears_all_startup_fields_even_when_intents_coexist() {
    let mut app = test_app();
    app.deferred_startup.session =
        Some(crate::app::session_startup::DeferredSessionStartup::Load {
            session_id: "deferred-session".into(),
            session_cwd: None,
            chat_kind: false,
        });
    app.deferred_startup.new_session = true;
    app.deferred_startup.worktree_label = Some("stray".into());
    let effects = drain_startup_actions(&mut app);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::LoadSession { .. })),
        "the higher-priority deferred resume replays",
    );
    assert!(app.deferred_startup.session.is_none());
    assert!(app.deferred_startup.session.is_none());
    assert!(!app.deferred_startup.worktree);
    assert!(app.deferred_startup.worktree_label.is_none());
    assert!(
        !app.deferred_startup.new_session,
        "the superseded Ctrl+N intent must be cleared, not left set",
    );
    assert!(app.deferred_startup.prompt.is_none());
    assert!(!app.deferred_startup.open_dashboard);
}
/// The `--worktree <ref>` feature must keep
/// working through the startup gate. A `NewWorktreeSession` carrying a
/// `git_ref` dispatched while the trust gate is closed is stashed into
/// `deferred_startup.worktree_ref` by the chokepoint; once the gate opens the drain
/// replays it as the worktree's git ref and clears the field.
#[test]
fn deferred_worktree_ref_replays_through_gate() {
    let mut app = test_app();
    app.cwd_has_git_ancestor = true;
    app.trust_state = TrustState::Pending {
        workspace: PathBuf::from("/x"),
    };
    let effects = dispatch(
        Action::NewWorktreeSession {
            load_session_id: None,
            label: Some("wt".into()),
            git_ref: Some("feature-branch".into()),
        },
        &mut app,
    );
    assert!(
        effects.is_empty(),
        "a gated worktree session produces no effects",
    );
    assert_eq!(
        app.deferred_startup.worktree_ref.as_deref(),
        Some("feature-branch"),
        "the --worktree <ref> must be stashed for replay",
    );
    assert!(app.deferred_startup.worktree);
    let effects = finish_trust(&mut app);
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::CreateWorktreeSession {
                git_ref: Some(r),
                .. } if r == "feature-branch"
        )),
        "the deferred --worktree <ref> replays with its git ref",
    );
    assert!(
        app.deferred_startup.worktree_ref.is_none(),
        "the stashed worktree ref is consumed by the drain",
    );
    assert!(!app.deferred_startup.worktree);
}
/// Regression: a gated worktree-WITHOUT-load-id must
/// not clobber a `--resume`/`LoadSession` id a previous gated action already
/// stashed. Otherwise `drain_startup_actions` loses the resume intent and
/// opens a blank worktree instead of a worktree-with-resume.
#[test]
fn gated_worktree_without_load_id_preserves_stashed_resume() {
    let mut app = test_app();
    app.cwd_has_git_ancestor = true;
    app.trust_state = TrustState::Pending {
        workspace: PathBuf::from("/x"),
    };
    let effects = dispatch(
        Action::LoadSession("resume-me".into(), None, false),
        &mut app,
    );
    assert!(effects.is_empty(), "a gated resume produces no effects");
    assert_eq!(
        app.deferred_startup.session.as_ref().and_then(|d| match d {
            crate::app::session_startup::DeferredSessionStartup::Load { session_id, .. } =>
                Some(session_id.as_str()),
            _ => None,
        }),
        Some("resume-me"),
        "the gated resume id must be stashed",
    );
    let effects = dispatch(
        Action::NewWorktreeSession {
            load_session_id: None,
            label: None,
            git_ref: None,
        },
        &mut app,
    );
    assert!(effects.is_empty(), "a gated worktree produces no effects");
    assert_eq!(
        app.deferred_startup.session.as_ref().and_then(|d| match d {
            crate::app::session_startup::DeferredSessionStartup::Load { session_id, .. } =>
                Some(session_id.as_str()),
            _ => None,
        }),
        Some("resume-me"),
        "the worktree-without-load-id must preserve the stashed resume id",
    );
    assert!(
        app.deferred_startup.worktree,
        "the worktree intent is stashed too"
    );
    let effects = finish_trust(&mut app);
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::CreateWorktreeSession {
                load_session_id: Some(id),
                .. } if id == "resume-me"
        )),
        "the deferred worktree replays with the preserved resume id",
    );
    assert!(app.deferred_startup.session.is_none());
    assert!(!app.deferred_startup.worktree);
}
/// Regression (derisk pass): the same clobber class as the resume-id fix,
/// now for the worktree companion fields. A gated worktree dispatch carrying
/// `None` label/ref must NOT erase a `--worktree <label>` / `--worktree-ref
/// <ref>` a previous gated action already stashed -- otherwise the drain
/// drops the label and builds off the default branch instead of the ref.
#[test]
fn gated_worktree_with_none_companions_preserves_stashed_label_and_ref() {
    let mut app = test_app();
    app.cwd_has_git_ancestor = true;
    app.trust_state = TrustState::Pending {
        workspace: PathBuf::from("/x"),
    };
    let effects = dispatch(
        Action::NewWorktreeSession {
            load_session_id: Some("mysess".into()),
            label: Some("mylabel".into()),
            git_ref: Some("featbranch".into()),
        },
        &mut app,
    );
    assert!(effects.is_empty(), "a gated worktree produces no effects");
    assert!(matches!(
        app.deferred_startup.session.as_ref(),
        Some(crate::app::session_startup::DeferredSessionStartup::Load { session_id, .. }) if session_id == "mysess"
    ));
    assert_eq!(
        app.deferred_startup.worktree_label.as_deref(),
        Some("mylabel")
    );
    assert_eq!(
        app.deferred_startup.worktree_ref.as_deref(),
        Some("featbranch")
    );
    let effects = dispatch(
        Action::NewWorktreeSession {
            load_session_id: None,
            label: None,
            git_ref: None,
        },
        &mut app,
    );
    assert!(effects.is_empty(), "a gated worktree produces no effects");
    assert_eq!(
        app.deferred_startup.session.as_ref().and_then(|d| match d {
            crate::app::session_startup::DeferredSessionStartup::Load { session_id, .. } =>
                Some(session_id.as_str()),
            _ => None,
        }),
        Some("mysess"),
        "the blank worktree must preserve the stashed resume id",
    );
    assert_eq!(
        app.deferred_startup.worktree_label.as_deref(),
        Some("mylabel"),
        "the blank worktree must preserve the stashed label",
    );
    assert_eq!(
        app.deferred_startup.worktree_ref.as_deref(),
        Some("featbranch"),
        "the blank worktree must preserve the stashed git ref",
    );
    assert!(app.deferred_startup.worktree);
    let effects = finish_trust(&mut app);
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::CreateWorktreeSession {
                load_session_id: Some(id),
                label: Some(l),
                git_ref: Some(r),
                .. } if id == "mysess" && l == "mylabel" && r == "featbranch"
        )),
        "the deferred worktree replays with the preserved id, label, and ref",
    );
    assert!(app.deferred_startup.worktree_ref.is_none());
    assert!(app.deferred_startup.worktree_label.is_none());
    assert!(app.deferred_startup.session.is_none());
    assert!(!app.deferred_startup.worktree);
}
/// `/login` from inside a session must move to the welcome screen (the
/// only view that renders the auth flow / external-provider URL) and
/// stash the agent view for restoration. This is the core fix for the
/// "external auth provider /login does nothing mid-session" bug.
#[test]
fn login_mid_session_switches_to_welcome_and_stashes_view() {
    let mut app = test_app_with_agent();
    assert_eq!(app.active_view, ActiveView::Agent(AgentId(0)));
    let effects = dispatch(Action::Login, &mut app);
    assert_eq!(app.active_view, ActiveView::Welcome);
    assert_eq!(app.auth_return_view, Some(ActiveView::Agent(AgentId(0))));
    assert!(matches!(app.auth_state, AuthState::Authenticating { .. }));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Authenticate { .. })),
        "must still kick off the auth flow",
    );
}
/// A mid-session `/login` switches to the welcome view to host the auth
/// flow; that transition must collapse any expanded announcement so it
/// can't reappear stale if auth completion lands back on a welcome screen.
#[test]
fn login_mid_session_resets_welcome_announcement_expanded() {
    let mut app = test_app_with_agent();
    app.welcome_announcement.expanded = true;
    dispatch(Action::Login, &mut app);
    assert_eq!(app.active_view, ActiveView::Welcome);
    assert!(
        !app.welcome_announcement.expanded,
        "mid-session login must reset the expanded announcement"
    );
}
/// After a successful mid-session re-auth, the stale `ReAuthRequired`
/// prompt (pushed when the 401 surfaced) is stripped so the user
/// returns to a clean session.
#[test]
fn auth_complete_strips_reauth_prompt_after_mid_session_login() {
    use crate::scrollback::block::RenderBlock;
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents
        .get_mut(&id)
        .unwrap()
        .scrollback
        .push_block(RenderBlock::session_event(SessionEvent::ReAuthRequired));
    dispatch(Action::Login, &mut app);
    let seq = authenticating_seq(&app);
    dispatch(
        Action::TaskComplete(TaskResult::AuthComplete {
            request_seq: seq,
            meta: None,
        }),
        &mut app,
    );
    assert_eq!(app.active_view, ActiveView::Agent(id));
    let sb = &app.agents[&id].scrollback;
    let has_reauth = (0..sb.len()).any(|i| {
        matches!(
            sb.entry(i).map(|e| &e.block),
            Some(RenderBlock::SessionEvent(ev)) if matches!(ev.event, SessionEvent::ReAuthRequired)
        )
    });
    assert!(
        !has_reauth,
        "re-auth prompt must be stripped after successful re-auth"
    );
}
/// After a successful mid-session re-auth, the prompt that failed on the
/// expired login is auto-resubmitted so the user doesn't have to retype it.
#[test]
fn auth_complete_retries_stashed_prompt_after_mid_session_login() {
    use crate::scrollback::block::RenderBlock;
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent
            .scrollback
            .push_block(RenderBlock::session_event(SessionEvent::ReAuthRequired));
        agent.reauth_stashed_prompt = Some(crate::app::agent::InFlightPrompt {
            text: "retry me".into(),
            images: Vec::new(),
            scrollback_entry: crate::scrollback::EntryId::new(0),
            combined_scrollback_entries: Vec::new(),
            chip_elements: Vec::new(),
        });
    }
    dispatch(Action::Login, &mut app);
    let seq = authenticating_seq(&app);
    let effects = dispatch(
        Action::TaskComplete(TaskResult::AuthComplete {
            request_seq: seq,
            meta: None,
        }),
        &mut app,
    );
    assert_eq!(app.active_view, ActiveView::Agent(id));
    let agent = &app.agents[&id];
    assert!(
        agent.reauth_stashed_prompt.is_none(),
        "stashed prompt must be consumed on re-auth"
    );
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::SendPrompt { text, .. } if text == "retry me"
        )),
        "the stashed prompt must be auto-resubmitted, got: {effects:?}"
    );
}
#[test]
fn dispatch_new_session_answered_with_persist_never_updates_mode_and_emits_effect() {
    let mut app = new_session_test_app();
    app.new_session_worktree_mode = crate::app::app_view::WorktreeMode::Ask;
    let effects = dispatch(
        Action::NewSessionAnswered {
            worktree: false,
            persist_mode: Some(crate::app::app_view::WorktreeMode::Never),
        },
        &mut app,
    );
    assert_eq!(
        app.new_session_worktree_mode,
        crate::app::app_view::WorktreeMode::Never
    );
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::PersistWorktreeMode {
                config_key: "new_session_worktree_mode",
                ..
            }
        )),
        "expected PersistWorktreeMode with config_key new_session_worktree_mode"
    );
}
#[test]
fn dispatch_new_session_has_empty_scrollback() {
    let mut app = test_app_with_agent();
    dispatch(Action::NewSession, &mut app);
    let new_id = AgentId(1);
    assert_eq!(app.agents[&new_id].scrollback.len(), 0);
}
#[test]
fn translate_local_submit_always_returns_persist_always_for_new_session() {
    use crate::views::question_view::{LocalQuestionKind, QuestionViewState};
    use xai_grok_tools::implementations::grok_build::ask_user_question::{
        Question, QuestionOption,
    };
    let q = Question {
        question: "?".into(),
        options: (0..4)
            .map(|i| QuestionOption {
                label: format!("opt{i}"),
                description: String::new(),
                preview: None,
                id: None,
            })
            .collect(),
        multi_select: Some(false),
        id: None,
    };
    let mut state = QuestionViewState::new(
        "x".into(),
        vec![q],
        crate::views::prompt_widget::StashedPrompt::default(),
    )
    .with_local_kind(LocalQuestionKind::NewSession);
    state.selections[0] = crate::views::question_view::QuestionSelection::Single(Some(2));
    let kind = state.local_kind.take().unwrap();
    let outcome = crate::app::agent_view::translate_local_submit_for_test(&state, kind, false);
    match outcome {
        crate::app::app_view::InputOutcome::Action(Action::NewSessionAnswered {
            worktree,
            persist_mode,
        }) => {
            assert!(worktree);
            assert_eq!(
                persist_mode,
                Some(crate::app::app_view::WorktreeMode::Always)
            );
        }
        other => panic!("expected NewSessionAnswered with persist Always, got {other:?}"),
    }
}
#[test]
fn translate_local_submit_never_returns_persist_never_for_new_session() {
    use crate::views::question_view::{LocalQuestionKind, QuestionViewState};
    use xai_grok_tools::implementations::grok_build::ask_user_question::{
        Question, QuestionOption,
    };
    let q = Question {
        question: "?".into(),
        options: (0..4)
            .map(|i| QuestionOption {
                label: format!("opt{i}"),
                description: String::new(),
                preview: None,
                id: None,
            })
            .collect(),
        multi_select: Some(false),
        id: None,
    };
    let mut state = QuestionViewState::new(
        "x".into(),
        vec![q],
        crate::views::prompt_widget::StashedPrompt::default(),
    )
    .with_local_kind(LocalQuestionKind::NewSession);
    state.selections[0] = crate::views::question_view::QuestionSelection::Single(Some(3));
    let kind = state.local_kind.take().unwrap();
    let outcome = crate::app::agent_view::translate_local_submit_for_test(&state, kind, false);
    match outcome {
        crate::app::app_view::InputOutcome::Action(Action::NewSessionAnswered {
            worktree,
            persist_mode,
        }) => {
            assert!(!worktree);
            assert_eq!(
                persist_mode,
                Some(crate::app::app_view::WorktreeMode::Never)
            );
        }
        other => panic!("expected NewSessionAnswered with persist Never, got {other:?}"),
    }
}
#[test]
fn delete_session_action_emits_delete_effect() {
    let mut app = test_app_with_agent();
    open_session_picker_with(&mut app, vec![make_picker_entry("s1", "/repo")]);
    let effects = dispatch(
        Action::DeleteSession {
            source: "local".into(),
            session_id: "s1".into(),
            cwd: "/repo".into(),
        },
        &mut app,
    );
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::DeleteSession {
                source,
                session_id,
                cwd,
            }] if source == "local" && session_id == "s1" && cwd == "/repo"
        ),
        "DeleteSession action must emit exactly one matching DeleteSession effect"
    );
}
#[test]
fn entry_title_falls_back_to_short_session_id_when_no_prompt() {
    use crate::views::session_title::entry_title;
    let mut app = test_app_with_agent();
    if let Some(a) = app.agents.get_mut(&AgentId(0)) {
        a.session.session_id = Some("abcdef0123456".into());
    }
    let title = entry_title(&app.agents[&AgentId(0)]);
    assert_eq!(title, "session abcdef01");
}
#[test]
fn new_session_defers_create_session_for_non_project_dir() {
    let mut app = project_picker_app();
    let effects = dispatch(Action::NewSession, &mut app);
    assert!(app.agents.contains_key(&AgentId(0)));
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::CreateSession { .. })),
    );
}
#[test]
fn new_session_creates_session_for_project_dir() {
    let mut app = test_app();
    app.project_picker_shown = false;
    app.cwd = PathBuf::from("/Users/someone/my-project");
    let effects = dispatch(Action::NewSession, &mut app);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::CreateSession { .. })),
    );
}
#[tokio::test]
async fn project_selected_creates_session_and_sends_prompt() {
    let mut app = project_picker_app();
    dispatch(Action::NewSession, &mut app);
    let id = AgentId(0);
    let dir = std::env::temp_dir();
    let selected = dunce::canonicalize(&dir).unwrap_or(dir);
    let effects = dispatch(
        Action::ProjectSelected {
            path: selected.clone(),
            stashed_prompt: "hello".into(),
            disable_picker: false,
        },
        &mut app,
    );
    assert_eq!(app.agents[&id].session.cwd, selected);
    assert_eq!(app.cwd, selected);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::SetWorkingDir { path } if path == &selected))
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::CreateSession { cwd, .. } if cwd == &selected))
    );
    assert_eq!(app.agents[&id].session.queue_len(), 1);
}
#[test]
fn bg_task_killed_no_op_for_unknown_session() {
    let mut app = two_agent_app_with_bg_task();
    let effects = dispatch(
        Action::TaskComplete(TaskResult::BgTaskKilled {
            session_id: "nonexistent".into(),
            task_id: "task-B-1".into(),
            outcome: Some(xai_grok_tools::types::KillOutcome::AlreadyExited),
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    assert!(app.agents[&AgentId(1)].session.bg_tasks["task-B-1"].pending_kill);
}
/// Mutual exclusion: enabling always-approve via the dispatch seam clears
/// the per-session `auto_mode` display flag (yolo wins).
#[test]
fn set_yolo_on_clears_session_auto_mode() {
    use crate::app::actions::PermissionModeKind;
    let mut app = test_app_with_agent();
    dispatch(
        Action::SetPermissionMode(PermissionModeKind::Auto),
        &mut app,
    );
    assert!(
        app.agents[&AgentId(0)].session.is_auto(),
        "precondition: active agent is in auto"
    );
    dispatch(Action::SetYoloMode(true), &mut app);
    assert!(app.agents[&AgentId(0)].session.is_yolo());
    assert!(
        !app.agents[&AgentId(0)].session.is_auto(),
        "enabling always-approve must clear the per-session auto flag (yolo wins)"
    );
}
/// Gate OFF + policy pin: Plan exit lands on Normal (ask) but MUST still
/// push `SetSessionMode(Default)` so the agent leaves Plan — otherwise the
/// session stays in Plan while the UI reads Normal.
#[test]
fn cycle_mode_plan_exit_under_gate_off_pin_emits_set_session_mode() {
    let mut app = test_app_with_agent();
    app.auto_mode_gate = false;
    app.yolo_policy_block = Some(POLICY_WARNING);
    app.agents.get_mut(&AgentId(0)).unwrap().plan_mode_pending = Some(true);
    let effects = dispatch(Action::CycleMode, &mut app);
    assert!(
        !app.agents[&AgentId(0)].session.is_yolo(),
        "the pin must keep yolo off when exiting Plan"
    );
    assert_eq!(app.current_ui.permission_mode.as_deref(), Some("ask"));
    assert_eq!(app.agents[&AgentId(0)].plan_mode_pending, Some(false));
    assert_eq!(agent_toast(&app).as_deref(), Some(POLICY_WARNING));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::SetSessionMode { .. })),
        "Plan exit under the pin must still send SetSessionMode(Default), got {effects:?}"
    );
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::PersistPermissionMode {
                canonical: "ask",
                ..
            }
        )),
        "expected PersistPermissionMode(ask) under pin, got {effects:?}"
    );
}
/// Pre-session cycle (no session id yet) under the pin: Plan → Auto does
/// not stage yolo (auto is the next mode; pin applies on Auto → Always-Approve).
#[test]
fn cycle_mode_pre_session_blocked_by_policy_pin() {
    let mut app = test_app_with_agent();
    app.yolo_policy_block = Some(POLICY_WARNING);
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.session.session_id = None;
        agent.plan_mode_pending = Some(true);
        agent.deferred_session_mode = Some(xai_grok_tools::types::SessionMode::Plan);
    }
    let _ = dispatch(Action::CycleMode, &mut app);
    let agent = &app.agents[&AgentId(0)];
    assert!(
        !agent.session.is_yolo(),
        "pre-session Plan→Auto must not stage yolo under the pin"
    );
    assert_eq!(app.current_ui.permission_mode.as_deref(), Some("auto"));
    let _ = dispatch(Action::CycleMode, &mut app);
    let agent = &app.agents[&AgentId(0)];
    assert!(
        !agent.session.is_yolo(),
        "pre-session cycle must not stage yolo under the pin"
    );
    assert_eq!(agent.plan_mode_pending, Some(false));
    assert_eq!(agent.deferred_session_mode, None);
    assert_eq!(agent_toast(&app).as_deref(), Some(POLICY_WARNING));
}
/// Pre-session cycle from Plan with STALE `yolo_mode = true` (e.g. client
/// state restored from before the pin landed): resets to Normal with yolo
/// cleared (matching the with-session catch-all for the same Plan+yolo input)
/// so always-approve is not left active behind another banner.
#[test]
fn cycle_mode_pre_session_clears_stale_yolo_under_pin() {
    let mut app = test_app_with_agent();
    app.yolo_policy_block = Some(POLICY_WARNING);
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.session.session_id = None;
        agent.plan_mode_pending = Some(true);
        agent.deferred_session_mode = Some(xai_grok_tools::types::SessionMode::Plan);
        agent.session.yolo_mode = true;
    }
    let _ = dispatch(Action::CycleMode, &mut app);
    let agent = &app.agents[&AgentId(0)];
    assert!(
        !agent.session.is_yolo(),
        "stale pre-session yolo must be cleared so Normal is enforced, not just displayed"
    );
    assert!(!app.default_yolo, "clamp must clear the global mirror too");
    assert_eq!(agent.plan_mode_pending, Some(false));
    assert_eq!(agent.deferred_session_mode, None);
    assert_eq!(
        app.current_ui.permission_mode.as_deref(),
        Some("ask"),
        "Plan+yolo resets to Normal (matches the with-session catch-all), not always-approve"
    );
}
/// Pre-session (welcome screen / fresh tab): Shift+Tab must cycle the
/// mode locally (optimistic pending + deferred ACP push) AND kick off
/// session creation — without emitting duplicate CreateSession effects
/// on repeated presses.
#[test]
fn dispatch_cycle_mode_pre_session_cycles_locally_and_creates_session() {
    let mut app = test_app_with_agent();
    app.agents.get_mut(&AgentId(0)).unwrap().session.session_id = None;
    let effects = dispatch(Action::CycleMode, &mut app);
    let agent = &app.agents[&AgentId(0)];
    assert_eq!(
        agent.plan_mode_pending,
        Some(true),
        "pre-session Normal → Plan must set optimistic pending"
    );
    assert_eq!(
        agent.deferred_session_mode,
        Some(xai_grok_tools::types::SessionMode::Plan),
        "Plan must be deferred to SessionCreated"
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::CreateSession { .. })),
        "first press must create the session, got {effects:?}"
    );
    let effects = dispatch(Action::CycleMode, &mut app);
    let agent = &app.agents[&AgentId(0)];
    assert!(!agent.session.is_yolo(), "Plan → Auto must not enable yolo");
    assert_eq!(app.current_ui.permission_mode.as_deref(), Some("auto"));
    assert_eq!(agent.plan_mode_pending, Some(false));
    assert!(agent.deferred_session_mode.is_none());
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::CreateSession { .. })),
        "no duplicate CreateSession, got {effects:?}"
    );
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::PersistPermissionMode {
                canonical: "auto",
                session_id: None,
                ..
            }
        )),
        "pre-session Plan → Auto must persist the displayed mode, got {effects:?}"
    );
    let _ = dispatch(Action::CycleMode, &mut app);
    let agent = &app.agents[&AgentId(0)];
    assert!(agent.session.is_yolo(), "Auto → Always-Approve flips yolo");
    let _ = dispatch(Action::CycleMode, &mut app);
    let agent = &app.agents[&AgentId(0)];
    assert!(!agent.session.is_yolo());
    assert_eq!(agent.plan_mode_pending, Some(false));
}
/// No-session bail-out: when the active agent
/// has no ACP session, the setter toasts "No active session" and
/// returns empty effects. Pins the safety contract that we never
/// dispatch a mode change to a non-existent session.
#[test]
fn set_plan_mode_no_session_toasts_and_bails() {
    let mut app = test_app_with_agent();
    app.agents.get_mut(&AgentId(0)).unwrap().session.session_id = None;
    let effects = dispatch(
        Action::SetPlanMode(crate::app::actions::PlanModeKind::On),
        &mut app,
    );
    assert!(
        effects.is_empty(),
        "no-session bail must NOT emit Effect (no live session to set mode on)"
    );
    let toast = read_toast(&app);
    assert!(
        toast.contains("No active session"),
        "bail-out must surface the reason to the user: {toast}",
    );
    let agent = app.agents.get(&AgentId(0)).unwrap();
    assert!(agent.plan_mode_pending.is_none());
    assert!(!agent.plan_mode_active);
}
/// Non-idempotent ON transition: emits
/// `Effect::SetSessionMode(plan)` and sets `plan_mode_pending`.
/// Complement to the idempotent-ON test above.
#[test]
fn set_plan_mode_on_from_off_emits_set_session_mode() {
    let mut app = test_app_with_agent();
    let effects = dispatch(
        Action::SetPlanMode(crate::app::actions::PlanModeKind::On),
        &mut app,
    );
    assert_eq!(effects.len(), 1);
    assert!(
        matches!(&effects[0], Effect::SetSessionMode { mode_id, .. } if &*mode_id.0 == "plan"),
        "expected SetSessionMode(plan), got: {effects:?}"
    );
    let agent = app.agents.get(&AgentId(0)).unwrap();
    assert_eq!(
        agent.plan_mode_pending,
        Some(true),
        "optimistic pending must be set to Some(true)"
    );
}
/// Real-world repro: the peek panel is OPEN for the selected row
/// (it auto-opens on render), and the close is driven END-TO-END
/// through `handle_input` (Ctrl+X twice) exactly as the event loop
/// does. Verifies the selection moves to the next row AND the peek
/// follows it — the path the direct-`dispatch_dashboard_stop` tests
/// above don't exercise.
#[serial_test::serial(GROK_AGENT_DASHBOARD)]
#[test]
fn dashboard_stop_with_peek_open_moves_selection_and_peek_down_one() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    let mut app = test_app();
    let _ = dispatch_new_session_inner(&mut app, None);
    let _ = dispatch_new_session_inner(&mut app, None);
    let _ = dispatch_new_session_inner(&mut app, None);
    for (i, agent) in app.agents.values_mut().enumerate() {
        agent.display_name = Some(format!("agent-{i}"));
    }
    open_dashboard(&mut app);
    let order = dashboard_row_order(&app);
    assert!(order.len() >= 3, "need >=3 rows, got {}", order.len());
    let first = order[0].clone();
    let second = order[1].clone();
    app.dashboard.as_mut().unwrap().focus_row(first.clone());
    let area = Rect::new(0, 0, 80, 40);
    let reg = crate::actions::ActionRegistry::defaults();
    let render = |app: &mut AppView| {
        let mut buf = Buffer::empty(area);
        let _ = crate::views::dashboard::render_dashboard(
            &mut buf,
            area,
            app.dashboard.as_mut().unwrap(),
            &mut app.agents,
            &reg,
            None,
            &[],
            false,
            None,
        );
    };
    render(&mut app);
    assert!(
        app.dashboard.as_ref().unwrap().peek.is_some(),
        "peek must auto-open for the selected row",
    );
    let ctrl_x = Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
    for _ in 0..2 {
        let outcome = app
            .dashboard
            .as_mut()
            .unwrap()
            .handle_input(&ctrl_x, &app.registry);
        match outcome {
            crate::app::app_view::InputOutcome::Action(
                crate::app::actions::Action::DashboardStop,
            ) => {
                let _ = dispatch(crate::app::actions::Action::DashboardStop, &mut app);
            }
            other => panic!("Ctrl+X must produce DashboardStop, got {other:?}"),
        }
    }
    let crate::views::dashboard::DashboardRowId::TopLevel(first_id) = first else {
        panic!("first row should be top-level");
    };
    assert!(
        !app.agents.contains_key(&first_id),
        "closed agent must be gone"
    );
    assert_eq!(
        app.dashboard.as_ref().unwrap().selected,
        Some(second.clone()),
        "closing with peek open should still select the next row down",
    );
    render(&mut app);
    assert_eq!(
        app.dashboard
            .as_ref()
            .unwrap()
            .peek
            .as_ref()
            .map(|p| p.row.clone()),
        Some(second),
        "peek must follow the selection onto the next row",
    );
}
/// Regression — the same Ctrl+X double-press path driven
/// END-TO-END through `DashboardState::handle_input` (which the
/// existing `dashboard_stop_double_press_closes_top_level` test
/// bypasses by calling `dispatch_dashboard_stop` directly).
///
/// Without the fix, the second `handle_input` call
/// runs the top-of-`handle_key` toast/confirm clear BEFORE the
/// registry resolves the key to `DashboardStop`, wiping the
/// just-armed `stop_confirm`. The dispatcher then sees a fresh
/// state and re-arms instead of closing. The session never closes
/// no matter how many times the user presses Ctrl+X.
#[serial_test::serial(GROK_AGENT_DASHBOARD)]
#[test]
fn dashboard_stop_double_press_via_handle_key_closes_top_level() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let mut app = test_app();
    let _ = dispatch_new_session_inner(&mut app, None);
    let _ = dispatch_new_session_inner(&mut app, None);
    open_dashboard(&mut app);
    let target = *app.agents.keys().next().unwrap();
    if let Some(d) = app.dashboard.as_mut() {
        d.selected = Some(crate::views::dashboard::DashboardRowId::TopLevel(target));
    }
    let ctrl_x = Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
    let outcome1 = app
        .dashboard
        .as_mut()
        .unwrap()
        .handle_input(&ctrl_x, &app.registry);
    match outcome1 {
        crate::app::app_view::InputOutcome::Action(crate::app::actions::Action::DashboardStop) => {
            let _ = dispatch(crate::app::actions::Action::DashboardStop, &mut app);
        }
        other => panic!("first Ctrl+X must produce DashboardStop, got {other:?}"),
    }
    assert!(
        app.dashboard.as_ref().unwrap().stop_confirm.is_some(),
        "first Ctrl+X must arm stop_confirm"
    );
    let outcome2 = app
        .dashboard
        .as_mut()
        .unwrap()
        .handle_input(&ctrl_x, &app.registry);
    match outcome2 {
        crate::app::app_view::InputOutcome::Action(crate::app::actions::Action::DashboardStop) => {
            let _ = dispatch(crate::app::actions::Action::DashboardStop, &mut app);
        }
        other => panic!("second Ctrl+X must produce DashboardStop, got {other:?}"),
    }
    assert!(
        !app.agents.contains_key(&target),
        "second Ctrl+X via handle_input must close the target agent (Issue 300 regression)",
    );
}
/// Top-level resolver round-trip via real AgentView.
#[test]
fn session_id_resolver_round_trip_top_level() {
    use crate::views::dashboard::{DashboardRowId, PersistedRowId, SessionIdResolver};
    let app = test_app_with_agent();
    let resolver = SessionIdResolver::from_agents(&app.agents);
    let pid = PersistedRowId::TopLevel {
        session_id: "test-session".into(),
    };
    let live = resolver.resolve(&pid).expect("must resolve");
    assert_eq!(live, DashboardRowId::TopLevel(AgentId(0)));
    let back = resolver.to_persisted(&live).expect("must reverse");
    assert_eq!(back, pid);
    let absent = PersistedRowId::TopLevel {
        session_id: "no-such-session".into(),
    };
    assert!(resolver.resolve(&absent).is_none());
}
/// Subagent resolver round-trip.
#[test]
fn session_id_resolver_round_trip_subagent() {
    use crate::views::dashboard::{DashboardRowId, PersistedRowId, SessionIdResolver};
    let mut app = test_app_with_agent();
    let agent = app.agents.get_mut(&AgentId(0)).unwrap();
    let info = make_test_subagent("child-1", "sa-1");
    agent
        .subagent_sessions
        .insert(info.child_session_id.to_string(), info);
    let resolver = SessionIdResolver::from_agents(&app.agents);
    let pid = PersistedRowId::Subagent {
        parent_session_id: "test-session".into(),
        child_session_id: "child-1".into(),
    };
    let live = resolver.resolve(&pid).expect("must resolve");
    assert_eq!(
        live,
        DashboardRowId::Subagent {
            parent: AgentId(0),
            child_session_id: "child-1".into(),
        }
    );
    let back = resolver.to_persisted(&live).expect("must reverse");
    assert_eq!(back, pid);
}
