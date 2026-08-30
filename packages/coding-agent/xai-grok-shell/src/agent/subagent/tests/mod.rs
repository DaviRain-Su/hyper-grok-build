#![cfg_attr(rustfmt, rustfmt::skip)]
use super::*;
use crate::session::SessionThread;
use super::spawn::{
    inject_subagent_completed_prompt, join_worker_task, present_child_completion,
    should_auto_wake_subagent, will_wake_for, AutoWakeInputs, InjectParams,
};
use super::attempt_runner::{
    canonical_total_tokens, record_subagent_usage, usage_is_incomplete,
};
use crate::test_support::lsp_runtime::{ctx_with_toggle, test_gateway_with_receiver};
use xai_grok_subagent_resolution::resolve_effective_overrides;
use xai_grok_tools::implementations::grok_build::task::coordinator::{
    ChildCompletion, CompletionDisposition,
};

#[test]
fn canonical_total_tokens_does_not_double_count_reasoning() {
    let totals = xai_chat_state::UsageTotals {
        input_tokens: 100,
        output_tokens: 40,
        reasoning_tokens: 25,
        ..Default::default()
    };
    assert_eq!(canonical_total_tokens(&totals), 140);
}
#[test]
fn cancellation_makes_an_otherwise_complete_usage_snapshot_incomplete() {
    assert!(usage_is_incomplete(false, true));
    assert!(!usage_is_incomplete(false, false));
    assert!(usage_is_incomplete(true, false));
}
#[test]
fn oracle_execution_budget_resolves_and_reserves_finalization_capacity() {
    let mut definition = xai_grok_agent::config::AgentDefinition::oracle();
    let budget = SubagentExecutionBudget::resolve(& definition, None);
    assert_eq!(budget.max_turns, Some(12));
    assert_eq!(budget.max_tool_calls, Some(40));
    assert_eq!(budget.timeout_secs, Some(180));
    assert_eq!(budget.finalize_grace_secs, Some(30));
    assert_eq!(budget.finalize_at_model_calls(), Some(11));
    assert_eq!(budget.finalize_at_tool_calls(), Some(32));
    assert_eq!(budget.finalize_at_elapsed(), Some(std::time::Duration::from_secs(150)));
    let wire = budget.wire().expect("Oracle is bounded");
    assert_eq!(wire.max_turns, Some(12));
    assert_eq!(wire.max_tool_calls, Some(40));
    append_execution_budget_prompt(& mut definition, budget);
    let prompt = definition.prompt_body.expect("Oracle prompt");
    assert!(prompt.contains("12 model/tool-use rounds"));
    assert!(prompt.contains("40 tool calls"));
    assert!(prompt.contains("180 seconds total wall-clock time"));
}
#[test]
fn partial_budget_results_require_new_plain_text_output() {
    assert!(can_use_partial_budget_result(true, "useful partial answer", false));
    assert!(! can_use_partial_budget_result(false, "answer", false));
    assert!(! can_use_partial_budget_result(true, "   ", false));
    assert!(
        ! can_use_partial_budget_result(true, r#"{"answer":"partial"}"#, true),
        "unvalidated schema output must not be reported as success"
    );
}
#[test]
fn unbounded_agent_does_not_gain_runtime_limits() {
    let definition = xai_grok_agent::config::AgentDefinition::general_purpose();
    let budget = SubagentExecutionBudget::resolve(& definition, None);
    assert!(budget.is_unbounded());
    assert!(budget.wire().is_none());
}
#[test]
fn budget_trigger_codes_and_reasons_are_stable() {
    for trigger in [
        SubagentBudgetTrigger::FinalizingTurns,
        SubagentBudgetTrigger::FinalizingToolCalls,
        SubagentBudgetTrigger::FinalizingTimeout,
        SubagentBudgetTrigger::MaxToolCalls,
        SubagentBudgetTrigger::Timeout,
    ] {
        assert_eq!(SubagentBudgetTrigger::from_code(trigger.code()), Some(trigger));
        assert!(! trigger.termination_reason().is_empty());
    }
    assert!(SubagentBudgetTrigger::MaxToolCalls.is_hard());
    assert!(SubagentBudgetTrigger::Timeout.is_hard());
    assert!(! SubagentBudgetTrigger::FinalizingTurns.is_hard());
}
#[tokio::test]
async fn usage_ack_precedes_terminal_presentation() {
    let mut ctx = ctx_with_toggle(HashMap::new());
    let (parent_cmd_tx, mut parent_cmd_rx) = mpsc::unbounded_channel();
    ctx.parent_cmd_tx = Some(parent_cmd_tx);
    let by_model = vec![(
            "test-model".to_string(),
            xai_chat_state::UsageTotals {
                input_tokens: 10,
                output_tokens: 4,
                ..Default::default()
            },
        )];
    let mut fold = Box::pin(
        record_subagent_usage(
            ctx.parent_cmd_tx.as_ref(),
            Some(by_model),
            Some("parent-prompt".to_string()),
            false,
        ),
    );
    let command = tokio::select! {
            command = parent_cmd_rx.recv() => command.expect("usage command"),
            result = &mut fold => panic!("usage fold returned before parent command: {result}"),
        };
    let SessionCommand::RecordSubagentUsage { respond_to, .. } = command else {
        panic!("expected RecordSubagentUsage");
    };
    assert!(
            tokio::time::timeout(std::time::Duration::ZERO, &mut fold)
                .await
                .is_err(),
            "child return must wait for usage acknowledgement"
        );
    assert!(parent_cmd_rx.try_recv().is_err());
    respond_to.send(()).expect("usage ack");
    assert!(fold.await);
    let (gateway, _gateway_rx) = test_gateway_with_receiver();
    let mut request = auto_wake_test_request("usage-order");
    request.run_in_background = false;
    let mut completion_data = ShellCompletionData::from_context(&ctx);
    completion_data.spawned_notification_emitted = true;
    let completion = ChildCompletion {
        request,
        result: SubagentResult {
            success: true,
            subagent_id: "usage-order".to_string(),
            child_session_id: "usage-order".to_string(),
            ..Default::default()
        },
        completion_data,
        disposition: CompletionDisposition {
            foreground_delivered: true,
            backgrounded: false,
            waiter_delivered: false,
            explicitly_killed: false,
            should_surface: false,
        },
    };
    let will_wake = will_wake_for(&completion);
    present_child_completion(completion, &gateway, will_wake);
    assert!(matches!(
            parent_cmd_rx.try_recv(),
            Ok(SessionCommand::XaiSessionNotification {
                notification: SessionNotification {
                    update: SessionUpdate::SubagentFinished { .. },
                    ..
                }
            })
        ));
}
/// Invariant: resolving a subagent applies the parent session's
/// `--tools`/`--disallowed-tools`/`--permission-mode` — driven through
/// `resolve_agent_definition` so the spawn path can't skip them.
#[tokio::test]
async fn subagent_inherits_session_cli_overrides() {
    use xai_grok_agent::config::{AgentDefinition, PermissionMode};
    let mut probe = AgentDefinition::general_purpose();
    probe.name = "session-override-probe".into();
    probe.permission_mode = PermissionMode::Plan;
    probe.disallowed_tools = vec!["write".into()];
    let mut config = crate::agent::config::Config::default();
    config.cli_agents = vec![probe];
    config.cli_agent_overrides = crate::agent::config::CliAgentOverrides {
        tools: Some(vec!["read_file".into(), "grep".into()]),
        disallowed_tools: Some(vec!["web_search".into(), "write".into()]),
        permission_mode: Some(PermissionMode::AcceptEdits),
        ..Default::default()
    };
    let mut ctx = ctx_with_toggle(std::collections::HashMap::new());
    ctx.agent_config = Some(config);
    let def = resolve_agent_definition("session-override-probe", &ctx)
        .expect("cli agent resolves");
    assert_eq!(
            def.session_tools_allowlist.as_deref(),
            Some(&["read_file".into(), "grep".into()][..])
        );
    assert_eq!(
            def.session_tools_denylist.as_deref(),
            Some(&["web_search".into(), "write".into()][..])
        );
    assert_eq!(def.disallowed_tools, vec!["write"]);
    assert_eq!(def.permission_mode, PermissionMode::AcceptEdits);
}
/// `permissionMode: bypassPermissions` is downgraded to `Default` under the
/// pin and honored without it; other modes and plugin stripping unaffected.
#[test]
fn subagent_bypass_permission_mode_gated_by_policy_pin() {
    use xai_grok_agent::config::PermissionMode;
    const PIN: &str = xai_grok_workspace::permission::resolution::YOLO_PIN_REASON_REQUIREMENTS;
    assert_eq!(
            resolve_subagent_permission_mode(PermissionMode::BypassPermissions, false, None),
            PermissionMode::BypassPermissions,
        );
    assert_eq!(
            resolve_subagent_permission_mode(PermissionMode::BypassPermissions, false, Some(PIN)),
            PermissionMode::Default,
        );
    assert_eq!(
            resolve_subagent_permission_mode(PermissionMode::Plan, false, Some(PIN)),
            PermissionMode::Plan,
        );
    assert_eq!(
            resolve_subagent_permission_mode(PermissionMode::BypassPermissions, true, None),
            PermissionMode::Default,
        );
}
/// Persisted⇒stamped chokepoint for the subagent emitter: the
/// `SessionCommand` persist hop and the live broadcast must carry the
/// SAME `eventId`, minted before the fork (divergent or missing ids
/// degrade cursor reconnects to full replays or re-applied lines).
#[tokio::test]
async fn emit_subagent_notification_stamps_one_event_id_on_both_paths() {
    use crate::test_support::lsp_runtime::test_gateway_with_receiver;
    let (gateway, mut gateway_rx) = test_gateway_with_receiver();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    emit_subagent_notification(
        &gateway,
        "parent-sess",
        SessionUpdate::SubagentFinished {
            subagent_id: "sa-1".into(),
            child_session_id: "child-1".into(),
            status: "completed".into(),
            error: None,
            termination_reason: None,
            usage: None,
            tool_calls: 0,
            turns: 0,
            duration_ms: 5,
            tokens_used: 0,
            output: None,
            will_wake: false,
        },
        Some(&cmd_tx),
    );
    let persisted_id = match cmd_rx.try_recv().expect("persist hop must fire") {
        SessionCommand::XaiSessionNotification { notification } => {
            notification
                .meta
                .as_ref()
                .and_then(|m| m.get("eventId"))
                .and_then(|v| v.as_str())
                .expect("persisted subagent lines must carry an eventId")
                .to_string()
        }
        _ => panic!("expected XaiSessionNotification"),
    };
    assert!(persisted_id.starts_with("parent-sess-"));
    let broadcast_id = match gateway_rx.try_recv().expect("broadcast must fire") {
        xai_acp_lib::AcpClientMessage::ExtNotification(args) => {
            let params: serde_json::Value = serde_json::from_str(
                    args.request.params.get(),
                )
                .unwrap();
            params["_meta"]["eventId"].as_str().unwrap().to_string()
        }
        _ => panic!("expected ExtNotification"),
    };
    assert_eq!(persisted_id, broadcast_id);
}
#[test]
fn subagent_max_turns_definition_wins_else_inherits_parent() {
    assert_eq!(super::resolve_subagent_max_turns(Some(2), Some(5)), Some(2));
    assert_eq!(super::resolve_subagent_max_turns(None, Some(5)), Some(5));
}
#[test]
fn resume_worktree_action_covers_three_outcomes() {
    use super::{ResumeWorktreeAction, resume_worktree_action};
    // Existing dir wins even when a snapshot is present.
    assert_eq!(
        resume_worktree_action(true, Some("refs/grok/subagents/x")),
        ResumeWorktreeAction::Reuse
    );
    assert_eq!(
        resume_worktree_action(false, Some("refs/grok/subagents/x")),
        ResumeWorktreeAction::Rehydrate
    );
    assert_eq!(
        resume_worktree_action(true, None),
        ResumeWorktreeAction::Reuse
    );
    assert_eq!(
        resume_worktree_action(false, None),
        ResumeWorktreeAction::Shared
    );
}

#[test]
fn validate_subagent_worktree_rejects_symlink_to_parent_cwd() {
    use super::validate_subagent_worktree_path;
    let tmp = tempfile::tempdir().unwrap();
    let parent = tmp.path().join("parent-cwd");
    let managed_base = tmp.path().join("managed-worktrees");
    std::fs::create_dir_all(&parent).unwrap();
    std::fs::create_dir_all(&managed_base).unwrap();
    std::fs::write(parent.join("secret.txt"), "parent data").unwrap();

    let link = managed_base.join("subagent-evil");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&parent, &link).unwrap();
    #[cfg(not(unix))]
    {
        if std::os::windows::fs::symlink_dir(&parent, &link).is_err() {
            return;
        }
    }

    let err = validate_subagent_worktree_path(&link, &parent, &parent, Some("evil")).unwrap_err();
    assert!(
        err.contains("symbolic link") || err.contains("symlink"),
        "expected symlink rejection, got: {err}"
    );
}

#[test]
fn validate_subagent_worktree_rejects_path_outside_managed_base() {
    use super::validate_subagent_worktree_path;
    let tmp = tempfile::tempdir().unwrap();
    let parent = tmp.path().join("parent");
    let external = tmp.path().join("subagent-x");
    std::fs::create_dir_all(&parent).unwrap();
    std::fs::create_dir_all(&external).unwrap();

    let err =
        validate_subagent_worktree_path(&external, &parent, &parent, Some("x")).unwrap_err();
    assert!(
        err.contains("outside managed")
            || err.contains("parent session cwd")
            || err.contains("isolation")
            || err.contains("basename"),
        "expected managed-base / parent rejection, got: {err}"
    );
}

/// Global lock for tests that mutate process environment (XDG_RUNTIME_DIR).
fn env_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Prepare the current process's managed temp worktree base as owner-only
/// (Unix 0700). Caller must hold [`env_test_lock`] for the whole setup+validate
/// window so `XDG_RUNTIME_DIR` cannot change between base selection and
/// `validate_subagent_worktree_path` (which re-resolves the same helper).
fn prepare_secure_temp_base() -> std::path::PathBuf {
    use super::subagent_temp_worktree_base;
    let base = subagent_temp_worktree_base();
    std::fs::create_dir_all(&base).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
        if let Err(e) = super::validate_unix_parent_chain(&base) {
            panic!(
                "prepare_secure_temp_base parent chain unsafe for {}: {e}",
                base.display()
            );
        }
        if let Err(e) = super::ensure_real_dir(&base) {
            panic!(
                "prepare_secure_temp_base leaf unsafe for {}: {e}",
                base.display()
            );
        }
    }
    base
}

#[test]
fn subagent_temp_worktree_base_is_per_uid_namespaced() {
    use super::subagent_temp_worktree_base;
    let base = subagent_temp_worktree_base();
    let name = base.file_name().and_then(|n| n.to_str()).unwrap_or("");
    // Per-UID temp/XDG leaf, or home `subagent-worktrees` fallback.
    assert!(
        name.starts_with("grok-subagent-worktrees-") || name == "subagent-worktrees",
        "temp base must be per-UID/user namespaced or home fallback, got {name}"
    );
    assert_ne!(
        name, "grok-subagent-worktrees",
        "must not use shared fixed name under /tmp"
    );
}

#[cfg(unix)]
#[test]
fn unix_mode_is_owner_only_pure() {
    use super::unix_mode_is_owner_only;
    assert!(unix_mode_is_owner_only(0o700));
    assert!(unix_mode_is_owner_only(0o600));
    assert!(unix_mode_is_owner_only(0o100_700)); // with file-type bits
    assert!(!unix_mode_is_owner_only(0o755));
    assert!(!unix_mode_is_owner_only(0o777));
    assert!(!unix_mode_is_owner_only(0o750));
    assert!(!unix_mode_is_owner_only(0o704));
}

#[cfg(unix)]
#[test]
fn unix_parent_component_policy_pure() {
    use super::{
        unix_mode_has_sticky, unix_mode_no_group_world_write, unix_parent_component_is_safe,
        unix_xdg_runtime_dir_mode_ok,
    };
    let euid = 1000u32;
    // Self-owned 0755 (no g/w write) ok.
    assert!(unix_parent_component_is_safe(euid, 0o755, euid));
    assert!(unix_mode_no_group_world_write(0o755));
    // Self-owned 0775 not ok.
    assert!(!unix_parent_component_is_safe(euid, 0o775, euid));
    assert!(!unix_mode_no_group_world_write(0o775));
    // Root-owned sticky /tmp (1777) ok.
    assert!(unix_mode_has_sticky(0o1777));
    assert!(unix_parent_component_is_safe(0, 0o1777, euid));
    // Root-owned 0777 without sticky not ok.
    assert!(!unix_parent_component_is_safe(0, 0o777, euid));
    // Other user not ok.
    assert!(!unix_parent_component_is_safe(1001, 0o755, euid));
    // XDG: 0700 ok, 0755 ok, 0777 not.
    assert!(unix_xdg_runtime_dir_mode_ok(0o700));
    assert!(unix_xdg_runtime_dir_mode_ok(0o755));
    assert!(!unix_xdg_runtime_dir_mode_ok(0o777));
}

/// RAII env var restore for tests.
struct EnvGuard {
    key: &'static str,
    prev: Option<String>,
}
impl EnvGuard {
    fn set(key: &'static str, val: &str) -> Self {
        let prev = std::env::var(key).ok();
        // SAFETY: single-threaded libtest default; tests that touch env should
        // not run in parallel with others that depend on the same key.
        unsafe { std::env::set_var(key, val) };
        Self { key, prev }
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => unsafe { std::env::set_var(self.key, v) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

#[cfg(unix)]
#[test]
fn xdg_runtime_dir_0777_is_not_used() {
    use super::subagent_temp_worktree_base;
    use std::os::unix::fs::PermissionsExt;
    let _lock = env_test_lock();
    let tmp = tempfile::tempdir().unwrap();
    let fake_xdg = tmp.path().join("xdg-insecure");
    std::fs::create_dir_all(&fake_xdg).unwrap();
    std::fs::set_permissions(&fake_xdg, std::fs::Permissions::from_mode(0o777)).unwrap();
    let _guard = EnvGuard::set("XDG_RUNTIME_DIR", fake_xdg.to_str().unwrap());
    let base = subagent_temp_worktree_base();
    assert!(
        !base.starts_with(&fake_xdg),
        "insecure XDG_RUNTIME_DIR must not be used: {}",
        base.display()
    );
}

#[cfg(unix)]
#[test]
fn xdg_runtime_dir_0700_is_used() {
    use super::subagent_temp_worktree_base;
    use std::os::unix::fs::PermissionsExt;
    let _lock = env_test_lock();
    let tmp = tempfile::tempdir().unwrap();
    // Put fake XDG under /tmp-style tree (parent chain: sticky /tmp ok).
    // tempfile is under /tmp so parents are root sticky or euid.
    let fake_xdg = tmp.path().join("xdg-secure");
    std::fs::create_dir_all(&fake_xdg).unwrap();
    std::fs::set_permissions(&fake_xdg, std::fs::Permissions::from_mode(0o700)).unwrap();
    // Also ensure the tempfile root itself is not group-writable if needed.
    if let Some(p) = fake_xdg.parent() {
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
    }
    let _guard = EnvGuard::set("XDG_RUNTIME_DIR", fake_xdg.to_str().unwrap());
    let base = subagent_temp_worktree_base();
    assert!(
        base.starts_with(&fake_xdg),
        "secure XDG_RUNTIME_DIR must be used: got {}, expected under {}",
        base.display(),
        fake_xdg.display()
    );
    let uid = unsafe { libc::geteuid() };
    assert!(
        base.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains(&uid.to_string())),
        "leaf should include uid"
    );
}

#[cfg(unix)]
#[test]
fn home_parent_0755_accepts_leaf_under_private_base() {
    use super::{unix_parent_component_is_safe, validate_subagent_worktree_path};
    use std::os::unix::fs::PermissionsExt;
    let _lock = env_test_lock();
    let euid = unsafe { libc::geteuid() };
    assert!(unix_parent_component_is_safe(euid, 0o755, euid));

    let parent = tempfile::tempdir().unwrap();
    let base = prepare_secure_temp_base();
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
    let id = "home-0755";
    let dest = base.join(format!("subagent-{id}"));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    let result = validate_subagent_worktree_path(&dest, parent.path(), parent.path(), Some(id));
    let _ = std::fs::remove_dir_all(&dest);
    assert!(
        result.is_ok(),
        "0755 parents + 0700 leaf should work: {result:?}"
    );
}

#[cfg(unix)]
#[test]
fn parent_chain_rejects_group_writable_component() {
    use super::validate_unix_parent_chain;
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let mid = tmp.path().join("gwrite");
    std::fs::create_dir_all(&mid).unwrap();
    std::fs::set_permissions(&mid, std::fs::Permissions::from_mode(0o775)).unwrap();
    let leaf = mid.join("child");
    let err = validate_unix_parent_chain(&leaf).unwrap_err();
    let _ = std::fs::set_permissions(&mid, std::fs::Permissions::from_mode(0o755));
    assert!(
        err.contains("not a safe parent") || err.contains("group/world") || err.contains("mode"),
        "0775 parent must be rejected: {err}"
    );
}

#[test]
fn validate_subagent_worktree_accepts_dir_under_temp_fallback() {
    use super::validate_subagent_worktree_path;
    let _lock = env_test_lock();
    let parent = tempfile::tempdir().unwrap();
    let id = "test-wt-accept";
    let base = prepare_secure_temp_base();
    let dest = base.join(format!("subagent-{id}"));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    let result = validate_subagent_worktree_path(&dest, parent.path(), parent.path(), Some(id));
    let _ = std::fs::remove_dir_all(&dest);
    assert!(
        result.is_ok(),
        "secure per-UID temp fallback worktree should be accepted: {result:?}"
    );
    if let Ok(identity) = result {
        assert_eq!(
            identity.path.file_name().and_then(|n| n.to_str()),
            Some(format!("subagent-{id}").as_str())
        );
    }
}

#[cfg(unix)]
#[test]
fn validate_subagent_worktree_rejects_world_writable_base() {
    use super::validate_subagent_worktree_path;
    use std::os::unix::fs::PermissionsExt;
    let _lock = env_test_lock();
    let parent = tempfile::tempdir().unwrap();
    let id = "world-base";
    let managed = prepare_secure_temp_base();
    // Temporarily make managed 0777.
    std::fs::set_permissions(&managed, std::fs::Permissions::from_mode(0o777)).unwrap();
    let dest2 = managed.join(format!("subagent-{id}"));
    let _ = std::fs::remove_dir_all(&dest2);
    std::fs::create_dir_all(&dest2).unwrap();
    let err =
        validate_subagent_worktree_path(&dest2, parent.path(), parent.path(), Some(id)).unwrap_err();
    // Restore secure perms for other tests.
    let _ = std::fs::set_permissions(&managed, std::fs::Permissions::from_mode(0o700));
    let _ = std::fs::remove_dir_all(&dest2);
    assert!(
        err.contains("group/world") || err.contains("0700") || err.contains("mode"),
        "0777 managed base must be rejected: {err}"
    );
}

#[cfg(unix)]
#[test]
fn validate_subagent_worktree_accepts_owner_only_0700_base() {
    use super::validate_subagent_worktree_path;
    use std::os::unix::fs::PermissionsExt;
    let _lock = env_test_lock();
    let parent = tempfile::tempdir().unwrap();
    let id = "safe-0700";
    let managed = prepare_secure_temp_base();
    std::fs::set_permissions(&managed, std::fs::Permissions::from_mode(0o700)).unwrap();
    let dest = managed.join(format!("subagent-{id}"));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    let result = validate_subagent_worktree_path(&dest, parent.path(), parent.path(), Some(id));
    let _ = std::fs::remove_dir_all(&dest);
    assert!(
        result.is_ok(),
        "0700 owner-only base must be accepted: {result:?}"
    );
}

#[test]
fn validate_subagent_worktree_rejects_wrong_agent_basename() {
    // Agent A metadata must not point at agent B's directory.
    use super::validate_subagent_worktree_path;
    let _lock = env_test_lock();
    let parent = tempfile::tempdir().unwrap();
    let id_a = "agent-a";
    let id_b = "agent-b";
    let base = prepare_secure_temp_base();
    let dest = base.join(format!("subagent-{id_b}"));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    let err =
        validate_subagent_worktree_path(&dest, parent.path(), parent.path(), Some(id_a)).unwrap_err();
    let _ = std::fs::remove_dir_all(&dest);
    assert!(
        err.contains("must be exactly") || err.contains("basename"),
        "expected basename identity rejection, got: {err}"
    );
}

#[test]
fn validate_subagent_worktree_rejects_unprefixed_legacy_name() {
    use super::validate_subagent_worktree_path;
    let _lock = env_test_lock();
    let parent = tempfile::tempdir().unwrap();
    let id = "legacy-id";
    // Unprefixed directory (legacy style) under temp base — must fail.
    let base = prepare_secure_temp_base();
    let dest = base.join(id);
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).unwrap();
    let err =
        validate_subagent_worktree_path(&dest, parent.path(), parent.path(), Some(id)).unwrap_err();
    let _ = std::fs::remove_dir_all(&dest);
    assert!(
        err.contains("must be exactly") || err.contains("subagent-"),
        "unprefixed legacy name must be rejected: {err}"
    );
}

#[cfg(unix)]
#[test]
fn validate_subagent_worktree_rejects_ancestor_symlink() {
    use super::validate_subagent_worktree_path;
    let _lock = env_test_lock();
    let tmp = tempfile::tempdir().unwrap();
    let parent = tmp.path().join("parent");
    std::fs::create_dir_all(&parent).unwrap();

    // Real leaf under a temporary real dir, then symlink the parent of dest
    // into the managed temp base path.
    let real_leaf_root = tmp.path().join("real-root");
    let real_leaf = real_leaf_root.join("subagent-anc");
    std::fs::create_dir_all(&real_leaf).unwrap();

    let managed = prepare_secure_temp_base();
    let link_name = managed.join("symlink-mid");
    let _ = std::fs::remove_file(&link_name);
    let _ = std::fs::remove_dir_all(&link_name);
    std::os::unix::fs::symlink(&real_leaf_root, &link_name).unwrap();
    let dest = link_name.join("subagent-anc");

    let err =
        validate_subagent_worktree_path(&dest, &parent, &parent, Some("anc")).unwrap_err();
    let _ = std::fs::remove_file(&link_name);
    assert!(
        err.contains("symbolic link") || err.contains("symlink"),
        "ancestor/mid-path symlink must be rejected: {err}"
    );
}

#[cfg(unix)]
#[test]
fn validate_subagent_worktree_rejects_dangling_base_symlink() {
    use super::validate_subagent_worktree_path;
    // Path component under managed base is a dangling symlink.
    let _lock = env_test_lock();
    let parent = tempfile::tempdir().unwrap();
    let managed = prepare_secure_temp_base();
    let dangling = managed.join("dangling-link");
    let _ = std::fs::remove_file(&dangling);
    let _ = std::fs::remove_dir_all(&dangling);
    std::os::unix::fs::symlink("/nonexistent/grok-wt-target-xyz", &dangling).unwrap();
    let dest = dangling.join("subagent-dangle");
    let err =
        validate_subagent_worktree_path(&dest, parent.path(), parent.path(), Some("dangle"))
            .unwrap_err();
    let _ = std::fs::remove_file(&dangling);
    assert!(
        err.contains("not accessible")
            || err.contains("symbolic link")
            || err.contains("symlink")
            || err.contains("cannot lstat"),
        "dangling base/component must fail closed: {err}"
    );
}

#[cfg(unix)]
#[test]
fn validate_subagent_worktree_inode_recheck_detects_replacement() {
    use super::validate_subagent_worktree_path;
    let _lock = env_test_lock();
    let parent = tempfile::tempdir().unwrap();
    let id = "inode-swap";
    let base = prepare_secure_temp_base();
    let dest = base.join(format!("subagent-{id}"));
    let alt = base.join(format!("subagent-{id}-alt"));
    let aside = base.join(format!("subagent-{id}-aside"));
    let _ = std::fs::remove_dir_all(&dest);
    let _ = std::fs::remove_dir_all(&alt);
    let _ = std::fs::remove_dir_all(&aside);
    std::fs::create_dir_all(&dest).unwrap();
    let identity =
        validate_subagent_worktree_path(&dest, parent.path(), parent.path(), Some(id)).unwrap();

    // Atomic-ish replacement: create a distinct directory then rename it into
    // place. remove+create can reuse the same inode on some filesystems.
    std::fs::create_dir_all(&alt).unwrap();
    std::fs::rename(&dest, &aside).unwrap();
    std::fs::rename(&alt, &dest).unwrap();

    let err = identity.matches_path(&dest).unwrap_err();
    assert!(
        err.contains("inode replaced") || err.contains("identity") || err.contains("changed"),
        "inode swap must be detected: {err}"
    );
    let _ = std::fs::remove_dir_all(&dest);
    let _ = std::fs::remove_dir_all(&aside);
}

#[test]
fn memory_injection_readonly_strips_write_tools_from_definition() {
    use super::apply_memory_tools_to_definition;
    use xai_grok_agent::config::AgentDefinition;
    use xai_grok_tools::registry::types::ToolConfig;
    use xai_tool_types::SubagentCapabilityMode;

    // Start with a definition that already smuggled write tools (from_id).
    let mut def = AgentDefinition::builtin_defaults("explore", "test");
    def.tool_config = xai_grok_tools::registry::types::ToolServerConfig {
        tools: vec![
            ToolConfig::from_id("write"),
            ToolConfig::from_id("search_replace"),
            ToolConfig::from_id("GrokBuild:read_file"),
        ],
        behavior_preset: None,
    };
    def.memory = Some(xai_grok_agent::config::MemoryScope::User);

    apply_memory_tools_to_definition(&mut def, Some(SubagentCapabilityMode::ReadOnly), false);

    let ids: Vec<&str> = def.tool_config.tools.iter().map(|t| t.id.as_str()).collect();
    assert!(
        !ids.iter().any(|id| {
            let short = id.rsplit(':').next().unwrap_or(id);
            matches!(short, "write" | "search_replace" | "search-replace")
        }),
        "ReadOnly final tool list must not contain write tools: {ids:?}"
    );
}

#[test]
fn memory_writes_allowed_respects_capability_ceiling() {
    use super::memory_writes_allowed;
    use xai_tool_types::SubagentCapabilityMode;

    assert!(
        memory_writes_allowed(None),
        "no ceiling: memory writes allowed"
    );
    assert!(memory_writes_allowed(Some(SubagentCapabilityMode::ReadWrite)));
    assert!(memory_writes_allowed(Some(SubagentCapabilityMode::All)));
    assert!(
        !memory_writes_allowed(Some(SubagentCapabilityMode::ReadOnly)),
        "read-only must not receive memory write tools"
    );
    assert!(
        !memory_writes_allowed(Some(SubagentCapabilityMode::Execute)),
        "execute-only must not receive memory write tools"
    );
}
#[test]
fn should_auto_wake_subagent_truth_table() {
    let wakeable = AutoWakeInputs {
        run_in_background: true,
        cancelled: false,
        auto_wake_enabled: true,
        block_waited: false,
        explicitly_killed: false,
        goal_loop_active: false,
        parent_channel_open: true,
    };
    assert!(should_auto_wake_subagent(wakeable));
    let suppressed = [
        AutoWakeInputs {
            run_in_background: false,
            ..wakeable
        },
        AutoWakeInputs {
            cancelled: true,
            ..wakeable
        },
        AutoWakeInputs {
            auto_wake_enabled: false,
            ..wakeable
        },
        AutoWakeInputs {
            block_waited: true,
            ..wakeable
        },
        AutoWakeInputs {
            explicitly_killed: true,
            ..wakeable
        },
        AutoWakeInputs {
            goal_loop_active: true,
            ..wakeable
        },
        AutoWakeInputs {
            parent_channel_open: false,
            ..wakeable
        },
    ];
    for (i, inputs) in suppressed.into_iter().enumerate() {
        assert!(!should_auto_wake_subagent(inputs), "suppressed case {i}");
    }
}
fn auto_wake_test_request(id: &str) -> SubagentRequest {
    SubagentRequest {
        id: id.into(),
        prompt: String::new(),
        description: "explore".into(),
        subagent_type: "general-purpose".into(),
        parent_session_id: "parent".into(),
        parent_prompt_id: None,
        resume_from: None,
        cwd: None,
        runtime_overrides: Default::default(),
        run_in_background: true,
        surface_completion: true,
        await_to_completion: false,
        fork_context: false,
        owner: SubagentOwner::Task,
        cancel_token: CancellationToken::new(),
    }
}
#[test]
fn inject_subagent_completed_prompt_sends_prompt_and_marks_delivered() {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCommand>();
    let reservations = xai_grok_tools::reminders::task_completion::TaskCompletionReservations::default();
    let mut request = auto_wake_test_request("sa-1");
    request.runtime_overrides.loop_task_id = Some("loop-123".into());
    let result = SubagentResult {
        success: true,
        subagent_id: "sa-1".into(),
        child_session_id: "sa-1".into(),
        ..Default::default()
    };
    reservations.reserve("sa-1".into());
    inject_subagent_completed_prompt(InjectParams {
        subagent_id: "sa-1",
        result: &result,
        request: &request,
        task_completion_reservations: &Some(reservations.clone()),
        parent_cmd_tx: Some(&cmd_tx),
        task_output_tool_name: "get_command_or_subagent_output",
        scheduler_delete_tool_name: Some("renamed_scheduler_delete"),
        synthetic_trace_tx: &None,
        goal_loop_active: &std::sync::atomic::AtomicBool::new(false),
    });
    match cmd_rx.try_recv().expect("expected synthetic Prompt") {
        SessionCommand::Prompt { prompt_id, prompt_blocks, verbatim, .. } => {
            assert!(prompt_id.starts_with("subagent-completed-"));
            assert!(verbatim);
            let prompt = prompt_blocks
                .iter()
                .filter_map(|block| match block {
                    acp::ContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            assert!(prompt.contains("renamed_scheduler_delete"));
            assert!(prompt.contains("loop-123"));
            assert!(prompt.contains("to stop the monitor"));
        }
        _ => panic!("expected SessionCommand::Prompt"),
    }
    assert_eq!(reservations.snapshot(), vec!["sa-1".to_string()]);
}
#[test]
fn inject_subagent_completed_prompt_omits_cleanup_without_loop_task() {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCommand>();
    let request = auto_wake_test_request("sa-no-loop");
    let result = SubagentResult {
        success: true,
        subagent_id: "sa-no-loop".into(),
        child_session_id: "sa-no-loop".into(),
        ..Default::default()
    };
    inject_subagent_completed_prompt(InjectParams {
        subagent_id: "sa-no-loop",
        result: &result,
        request: &request,
        task_completion_reservations: &None,
        parent_cmd_tx: Some(&cmd_tx),
        task_output_tool_name: "get_command_or_subagent_output",
        scheduler_delete_tool_name: Some("scheduler_delete"),
        synthetic_trace_tx: &None,
        goal_loop_active: &std::sync::atomic::AtomicBool::new(false),
    });
    let SessionCommand::Prompt { prompt_blocks, .. } = cmd_rx
        .try_recv()
        .expect("expected synthetic Prompt") else {
        panic!("expected SessionCommand::Prompt");
    };
    let prompt = prompt_blocks
        .iter()
        .filter_map(|block| match block {
            acp::ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(!prompt.contains("scheduler_delete"));
    assert!(!prompt.contains("to stop the monitor"));
}
#[test]
fn inject_subagent_completed_prompt_bails_when_goal_loop_activates_in_gap() {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCommand>();
    let reservations = xai_grok_tools::reminders::task_completion::TaskCompletionReservations::default();
    reservations.reserve("sa-goal".into());
    inject_subagent_completed_prompt(InjectParams {
        subagent_id: "sa-goal",
        result: &SubagentResult {
            success: true,
            subagent_id: "sa-goal".into(),
            child_session_id: "sa-goal".into(),
            ..Default::default()
        },
        request: &auto_wake_test_request("sa-goal"),
        task_completion_reservations: &Some(reservations.clone()),
        parent_cmd_tx: Some(&cmd_tx),
        task_output_tool_name: "get_command_or_subagent_output",
        scheduler_delete_tool_name: None,
        synthetic_trace_tx: &None,
        goal_loop_active: &std::sync::atomic::AtomicBool::new(true),
    });
    assert!(cmd_rx.try_recv().is_err(), "no prompt when the goal loop owns the cadence");
    assert!(!reservations.contains("sa-goal"), "this attempt's reservation must be released");
}
#[test]
fn inject_subagent_completed_prompt_releases_reservation_when_parent_closed() {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<SessionCommand>();
    drop(cmd_rx);
    let reservations = xai_grok_tools::reminders::task_completion::TaskCompletionReservations::default();
    reservations.reserve("sa-closed".into());
    reservations.reserve("sa-closed".into());
    let (trace_tx, mut trace_rx) = mpsc::unbounded_channel();
    inject_subagent_completed_prompt(InjectParams {
        subagent_id: "sa-closed",
        result: &SubagentResult {
            success: true,
            subagent_id: "sa-closed".into(),
            child_session_id: "sa-closed".into(),
            ..Default::default()
        },
        request: &auto_wake_test_request("sa-closed"),
        task_completion_reservations: &Some(reservations.clone()),
        parent_cmd_tx: Some(&cmd_tx),
        task_output_tool_name: "get_command_or_subagent_output",
        scheduler_delete_tool_name: None,
        synthetic_trace_tx: &Some(trace_tx),
        goal_loop_active: &std::sync::atomic::AtomicBool::new(false),
    });
    assert!(
            reservations.contains("sa-closed"),
            "send failure must release only the reservation acquired by this attempt"
        );
    reservations.release("sa-closed");
    assert!(!reservations.contains("sa-closed"));
    assert!(trace_rx.try_recv().is_err());
}
#[test]
fn persist_gate_only_persists_successful_nonempty_outputs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ok = SubagentResult {
        success: true,
        output: std::sync::Arc::from("text"),
        ..Default::default()
    };
    assert_eq!(
            persist_subagent_output(dir.path(), &ok),
            Some(dir.path().to_path_buf())
        );
    let empty = SubagentResult {
        success: true,
        ..Default::default()
    };
    assert_eq!(persist_subagent_output(dir.path(), &empty), None);
    let failed = SubagentResult {
        success: false,
        output: std::sync::Arc::from("partial"),
        ..Default::default()
    };
    assert_eq!(persist_subagent_output(dir.path(), &failed), None);
}
#[test]
fn subagent_output_roundtrips_through_output_json() {
    let dir = tempfile::tempdir().expect("tempdir");
    let output = "line one\nline two with unicode ✓";
    assert!(write_subagent_output(dir.path(), output));
    assert_eq!(read_subagent_output(dir.path()).as_deref(), Some(output));
    // Prefer plain markdown artifact for agent:// reads.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("output.md"))
            .ok()
            .as_deref(),
        Some(output)
    );
    assert_eq!(read_subagent_output(&dir.path().join("missing")), None);
    // Without markdown, corrupt JSON fails closed.
    let _ = std::fs::remove_file(dir.path().join("output.md"));
    std::fs::write(dir.path().join("output.json"), "not json").expect("corrupt file");
    assert_eq!(read_subagent_output(dir.path()), None);
}
#[test]
fn partial_override_fills_from_role() {
    let overrides = SubagentRuntimeOverrides {
        model: Some("explicit-model".into()),
        ..Default::default()
    };
    let role = xai_grok_subagent_resolution::config::SubagentRole {
        description: "test".into(),
        default_capability_mode: Some("execute".into()),
        ..Default::default()
    };
    let resolved = resolve_effective_overrides(
        &overrides,
        Some(&role),
        &HashMap::new(),
        None,
        None,
    );
    assert_eq!(resolved.model.as_deref(), Some("explicit-model"));
    assert_eq!(
            resolved.capability_mode,
            Some(xai_tool_types::SubagentCapabilityMode::Execute)
        );
}
#[test]
fn reasoning_effort_explicit_overrides_role() {
    let overrides = SubagentRuntimeOverrides {
        reasoning_effort: Some(xai_tool_types::SubagentReasoningEffort::High),
        ..Default::default()
    };
    let role = xai_grok_subagent_resolution::config::SubagentRole {
        description: "test".into(),
        reasoning_effort: Some(xai_tool_types::SubagentReasoningEffort::Low),
        ..Default::default()
    };
    let resolved = resolve_effective_overrides(
        &overrides,
        Some(&role),
        &HashMap::new(),
        None,
        None,
    );
    assert_eq!(
        resolved.reasoning_effort,
        Some(xai_tool_types::SubagentReasoningEffort::High)
    );
}
#[test]
fn reasoning_effort_falls_back_to_role() {
    let overrides = SubagentRuntimeOverrides::default();
    let role = xai_grok_subagent_resolution::config::SubagentRole {
        description: "test".into(),
        reasoning_effort: Some(xai_tool_types::SubagentReasoningEffort::Medium),
        ..Default::default()
    };
    let resolved = resolve_effective_overrides(
        &overrides,
        Some(&role),
        &HashMap::new(),
        None,
        None,
    );
    assert_eq!(
        resolved.reasoning_effort,
        Some(xai_tool_types::SubagentReasoningEffort::Medium)
    );
}
#[test]
fn invalid_role_capability_mode_ignored() {
    let overrides = SubagentRuntimeOverrides::default();
    let role = xai_grok_subagent_resolution::config::SubagentRole {
        description: "test".into(),
        default_capability_mode: Some("invalid-mode".into()),
        ..Default::default()
    };
    let resolved = resolve_effective_overrides(
        &overrides,
        Some(&role),
        &HashMap::new(),
        None,
        None,
    );
    assert!(
            resolved.capability_mode.is_none(),
            "invalid role mode should not produce a capability_mode"
        );
}
#[test]
fn persona_resolved_from_config() {
    let overrides = SubagentRuntimeOverrides {
        persona: Some("researcher".into()),
        ..Default::default()
    };
    let mut personas = HashMap::new();
    personas
        .insert(
            "researcher".to_string(),
            xai_grok_subagent_resolution::config::SubagentPersona {
                instructions: Some("Be thorough.".into()),
                ..Default::default()
            },
        );
    let resolved = resolve_effective_overrides(&overrides, None, &personas, None, None);
    assert_eq!(resolved.persona.as_deref(), Some("researcher"));
    assert_eq!(
            resolved.persona_instructions.as_deref(),
            Some("Be thorough.")
        );
}
#[test]
fn persona_inline_plus_file_merged_in_order() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("extra.md"), "File-based content.").unwrap();
    let overrides = SubagentRuntimeOverrides {
        persona: Some("combo".into()),
        ..Default::default()
    };
    let mut personas = HashMap::new();
    personas
        .insert(
            "combo".to_string(),
            xai_grok_subagent_resolution::config::SubagentPersona {
                instructions: Some("Inline first.".into()),
                instructions_file: Some("extra.md".into()),
                ..Default::default()
            },
        );
    let resolved = resolve_effective_overrides(
        &overrides,
        None,
        &personas,
        Some(tmp.path()),
        None,
    );
    let pi = resolved.persona_instructions.as_deref().unwrap();
    assert!(
            pi.starts_with("Inline first."),
            "inline should come first: {pi}"
        );
    assert!(
            pi.contains("File-based content."),
            "file content should be included: {pi}"
        );
}
#[test]
fn model_precedence_explicit_over_role_over_persona() {
    let mut personas = HashMap::new();
    personas
        .insert(
            "dev".to_string(),
            xai_grok_subagent_resolution::config::SubagentPersona {
                model: Some("persona-model".into()),
                ..Default::default()
            },
        );
    let role = xai_grok_subagent_resolution::config::SubagentRole {
        description: "test".into(),
        model: Some("role-model".into()),
        ..Default::default()
    };
    let overrides = SubagentRuntimeOverrides {
        persona: Some("dev".into()),
        model: Some("explicit-model".into()),
        ..Default::default()
    };
    let r = resolve_effective_overrides(&overrides, Some(&role), &personas, None, None);
    assert_eq!(r.model.as_deref(), Some("explicit-model"));
    let overrides = SubagentRuntimeOverrides {
        persona: Some("dev".into()),
        ..Default::default()
    };
    let r = resolve_effective_overrides(&overrides, Some(&role), &personas, None, None);
    assert_eq!(r.model.as_deref(), Some("role-model"));
    let role_no_model = xai_grok_subagent_resolution::config::SubagentRole {
        description: "test".into(),
        ..Default::default()
    };
    let r = resolve_effective_overrides(
        &overrides,
        Some(&role_no_model),
        &personas,
        None,
        None,
    );
    assert_eq!(r.model.as_deref(), Some("persona-model"));
    let overrides = SubagentRuntimeOverrides::default();
    let r = resolve_effective_overrides(&overrides, None, &HashMap::new(), None, None);
    assert!(r.model.is_none());
}
#[test]
fn reasoning_effort_precedence_explicit_over_role_over_persona() {
    let mut personas = HashMap::new();
    personas
        .insert(
            "dev".to_string(),
            xai_grok_subagent_resolution::config::SubagentPersona {
                reasoning_effort: Some(xai_tool_types::SubagentReasoningEffort::Low),
                ..Default::default()
            },
        );
    let role = xai_grok_subagent_resolution::config::SubagentRole {
        description: "test".into(),
        reasoning_effort: Some(xai_tool_types::SubagentReasoningEffort::Medium),
        ..Default::default()
    };
    let overrides = SubagentRuntimeOverrides {
        persona: Some("dev".into()),
        reasoning_effort: Some(xai_tool_types::SubagentReasoningEffort::High),
        ..Default::default()
    };
    let r = resolve_effective_overrides(&overrides, Some(&role), &personas, None, None);
    assert_eq!(r.reasoning_effort, Some(xai_tool_types::SubagentReasoningEffort::High));
    let overrides = SubagentRuntimeOverrides {
        persona: Some("dev".into()),
        ..Default::default()
    };
    let r = resolve_effective_overrides(&overrides, Some(&role), &personas, None, None);
    assert_eq!(r.reasoning_effort, Some(xai_tool_types::SubagentReasoningEffort::Medium));
    let role_no_re = xai_grok_subagent_resolution::config::SubagentRole {
        description: "test".into(),
        ..Default::default()
    };
    let r = resolve_effective_overrides(
        &overrides,
        Some(&role_no_re),
        &personas,
        None,
        None,
    );
    assert_eq!(r.reasoning_effort, Some(xai_tool_types::SubagentReasoningEffort::Low));
    let overrides = SubagentRuntimeOverrides::default();
    let r = resolve_effective_overrides(&overrides, None, &HashMap::new(), None, None);
    assert!(r.reasoning_effort.is_none());
}
#[test]
fn persona_not_found_produces_error() {
    let overrides = SubagentRuntimeOverrides {
        persona: Some("missing".into()),
        ..Default::default()
    };
    let resolved = resolve_effective_overrides(
        &overrides,
        None,
        &HashMap::new(),
        None,
        None,
    );
    assert!(resolved.persona_error.is_some());
    assert!(
            resolved
                .persona_error
                .as_deref()
                .unwrap()
                .contains("not found"),
        );
}
#[test]
fn prompt_assembly_ordering() {
    let role_prompt = Some(
        "<role-instructions>\nRole content\n</role-instructions>".to_string(),
    );
    let persona_instructions = Some(
        "<persona>\nPersona content\n</persona>".to_string(),
    );
    let task = "Do the task";
    let mut sections = Vec::new();
    sections.push("<fork-context>...</fork-context>".to_string());
    if let Some(ref rp) = role_prompt {
        sections.push(rp.clone());
    }
    if let Some(ref pi) = persona_instructions {
        sections.push(pi.clone());
    }
    sections.push(task.to_string());
    let assembled = sections.join("\n\n");
    let fork_pos = assembled.find("<fork-context>").unwrap();
    let role_pos = assembled.find("<role-instructions>").unwrap();
    let persona_pos = assembled.find("<persona>").unwrap();
    let task_pos = assembled.find("Do the task").unwrap();
    assert!(fork_pos < role_pos, "fork before role");
    assert!(role_pos < persona_pos, "role before persona");
    assert!(persona_pos < task_pos, "persona before task");
}
#[test]
fn no_persona_produces_none() {
    let overrides = SubagentRuntimeOverrides::default();
    let resolved = resolve_effective_overrides(
        &overrides,
        None,
        &HashMap::new(),
        None,
        None,
    );
    assert!(resolved.persona.is_none());
    assert!(resolved.persona_instructions.is_none());
}
#[test]
fn forked_initial_context_normalizes_parent_history() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let items = vec![
            ConversationItem::system("parent system"),
            ConversationItem::user("UNIQUE_FORK_MARKER_abc123 implement multi-repo fix"),
            ConversationItem::assistant("noted"),
        ];
    let ctx = forked_initial_context(items);
    assert_eq!(ctx.source, InitialContextSource::Forked);
    assert!(ctx.copy_error.is_none());
    assert_eq!(ctx.prefix_len, Some(2));
    assert_eq!(ctx.conversation.len(), 2);
    if let ConversationItem::User(ref u) = ctx.conversation[1] {
        let text: String = u
            .content
            .iter()
            .filter_map(|p| match p {
                xai_grok_sampling_types::conversation::ContentPart::Text { text } => {
                    Some(text.as_ref())
                }
                _ => None,
            })
            .collect();
        assert!(text.contains("<background_context>"));
        assert!(
                text.contains("UNIQUE_FORK_MARKER_abc123"),
                "distinctive parent token must appear in background: {text}"
            );
    } else {
        panic!("expected User background at [1]");
    }
}
#[test]
fn forked_initial_context_inherits_parent_across_reasoning() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let items = vec![
            ConversationItem::system("parent system"),
            ConversationItem::user("remember UNIQUE_FORK_MARKER_TEST"),
            ConversationItem::Reasoning(xai_grok_sampling_types::synthesized_reasoning_item(
                "deliberating",
            )),
            ConversationItem::assistant("ack"),
        ];
    let ctx = forked_initial_context(items);
    assert_eq!(ctx.source, InitialContextSource::Forked);
    assert_eq!(ctx.prefix_len, Some(2));
    assert_eq!(ctx.conversation.len(), 2);
    if let ConversationItem::User(ref u) = ctx.conversation[1] {
        let text: String = u
            .content
            .iter()
            .filter_map(|p| match p {
                xai_grok_sampling_types::conversation::ContentPart::Text { text } => {
                    Some(text.as_ref())
                }
                _ => None,
            })
            .collect();
        assert!(
                text.contains("<background_context>"),
                "background wrapper must be present: {text}"
            );
        assert!(
                text.contains("UNIQUE_FORK_MARKER_TEST"),
                "parent context must be inherited across the reasoning sibling: {text}"
            );
    } else {
        panic!("expected User background at [1]");
    }
}
#[test]
fn forked_initial_context_empty_fails_open_to_new() {
    let ctx = forked_initial_context(vec![]);
    assert_eq!(ctx.source, InitialContextSource::New);
    assert!(ctx.conversation.is_empty());
    assert!(ctx.copy_error.is_some());
}
#[test]
fn resume_vs_fork_helper_shapes_differ() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let resume_items = vec![
            ConversationItem::system("child system"),
            ConversationItem::user("prior subagent work"),
            ConversationItem::assistant("done"),
        ];
    let resumed = resume_initial_context(resume_items.clone());
    let forked = forked_initial_context(resume_items);
    assert_eq!(resumed.source, InitialContextSource::Resumed);
    assert_eq!(forked.source, InitialContextSource::Forked);
    assert!(resumed.conversation.len() > forked.conversation.len());
    assert!(!matches!(
            resumed.conversation.get(1),
            Some(ConversationItem::User(u))
                if u.content.iter().any(|p| matches!(
                    p,
                    xai_grok_sampling_types::conversation::ContentPart::Text { text }
                        if text.contains("<background_context>")
                ))
        ));
}
#[test]
fn forked_initial_context_applies_fork_filter_before_normalize() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let items = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("complete user"),
            ConversationItem::assistant("complete asst"),
            ConversationItem::user("INCOMPLETE_TRAILING"),
        ];
    let ctx = forked_initial_context(items);
    assert_eq!(ctx.source, InitialContextSource::Forked);
    if let ConversationItem::User(ref u) = ctx.conversation[1] {
        let text: String = u
            .content
            .iter()
            .filter_map(|p| match p {
                xai_grok_sampling_types::conversation::ContentPart::Text { text } => {
                    Some(text.as_ref())
                }
                _ => None,
            })
            .collect();
        assert!(text.contains("complete user"));
        assert!(
                !text.contains("INCOMPLETE_TRAILING"),
                "fork_filter must truncate incomplete trailing turn: {text}"
            );
    } else {
        panic!("expected background user");
    }
}
#[test]
fn verbatim_fork_keeps_items_byte_for_byte_when_small() {
    use xai_grok_sampling_types::conversation::{
        ContentPart, ConversationItem, SyntheticReason, UserItem,
    };
    let items = vec![
            ConversationItem::system("parent system"),
            ConversationItem::user("remember UNIQUE_FORK_MARKER_TEST"),
            ConversationItem::User(UserItem {
                content: vec![ContentPart::Text {
                    text: "SYNTHETIC_KEEP_ME".into(),
                }],
                synthetic_reason: Some(SyntheticReason::SystemReminder),
                ..Default::default()
            }),
            ConversationItem::Reasoning(xai_grok_sampling_types::synthesized_reasoning_item(
                "thinking",
            )),
            ConversationItem::assistant("ack"),
        ];
    let ctx = verbatim_or_normalize_fork(items, 256_000);
    assert_eq!(ctx.source, InitialContextSource::Forked);
    assert!(
            ctx.verbatim_fork,
            "a small, complete-tail parent must mirror verbatim"
        );
    assert_eq!(ctx.prefix_len, Some(5));
    assert_eq!(ctx.conversation.len(), 5);
    assert!(matches!(ctx.conversation[0], ConversationItem::System(_)));
    assert!(matches!(
            ctx.conversation.last(),
            Some(ConversationItem::Assistant(_))
        ));
    let text_present = |needle: &str| {
        ctx
            .conversation
            .iter()
            .any(|i| {
                matches!(i, ConversationItem::User(u)
                    if u.content.iter().any(|p| matches!(p,
                        ContentPart::Text { text } if text.contains(needle))))
            })
    };
    assert!(
            text_present("UNIQUE_FORK_MARKER_TEST"),
            "marker must survive verbatim"
        );
    assert!(
            text_present("SYNTHETIC_KEEP_ME"),
            "synthetic-reason item must be preserved verbatim, NOT stripped"
        );
    assert!(
            ctx.conversation
                .iter()
                .any(|i| matches!(i, ConversationItem::User(u) if u.synthetic_reason.is_some())),
            "the synthetic_reason marker itself must remain in the verbatim mirror"
        );
    assert!(
            !text_present("<background_context>"),
            "verbatim fork must NOT summarize into a background blob"
        );
}
#[test]
fn verbatim_fork_falls_back_to_summary_on_incomplete_tail() {
    use xai_grok_sampling_types::conversation::{
        AssistantItem, ContentPart, ConversationItem, ToolCall,
    };
    let items = vec![
        ConversationItem::system("parent system"),
        ConversationItem::user("q1 UNIQUE_FORK_MARKER_TEST"),
        ConversationItem::assistant("a1"),
        ConversationItem::user("q2"),
        ConversationItem::Assistant(AssistantItem {
            content: String::new().into(),
            provider_native_state: None,
            tool_calls: vec![ToolCall {
                id: "tc1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            }],
            model_id: None,
            reasoning_model_identity: None,
            model_fingerprint: None,
            reasoning_effort: None,
        }),
    ];
    let ctx = verbatim_or_normalize_fork(items, 256_000);
    assert_eq!(ctx.source, InitialContextSource::Forked);
    assert!(
            !ctx.verbatim_fork,
            "an incomplete (dangling tool call) tail must fall back to summary"
        );
    assert_eq!(ctx.prefix_len, Some(2));
    assert!(
            ctx.conversation.iter().any(|i| {
                matches!(i, ConversationItem::User(u)
                    if u.content.iter().any(|p| matches!(p,
                        ContentPart::Text { text } if text.contains("<background_context>"))))
            }),
            "summarized fallback must produce a background_context blob"
        );
}
#[test]
fn summarized_fork_is_not_a_verbatim_mirror() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let items = vec![
            ConversationItem::system("parent system prompt"),
            ConversationItem::user("turn one UNIQUE_FORK_MARKER_TEST"),
            ConversationItem::assistant("ack"),
        ];
    let ctx = verbatim_or_normalize_fork(items, 1);
    assert_eq!(ctx.source, InitialContextSource::Forked);
    assert!(!ctx.verbatim_fork);
    let verbatim_mirror_fork = ctx.source == InitialContextSource::Forked
        && ctx.verbatim_fork;
    assert!(
            !verbatim_mirror_fork,
            "a summarized fork must NOT be treated as a verbatim mirror"
        );
}
#[test]
fn verbatim_fork_falls_back_to_summary_when_oversize() {
    use xai_grok_sampling_types::conversation::{ContentPart, ConversationItem};
    let items = vec![
            ConversationItem::system("parent system"),
            ConversationItem::user("turn one UNIQUE_FORK_MARKER_TEST with some text"),
            ConversationItem::assistant("ack one"),
        ];
    let ctx = verbatim_or_normalize_fork(items, 1);
    assert_eq!(ctx.source, InitialContextSource::Forked);
    assert!(
            !ctx.verbatim_fork,
            "oversize parent must fall back to summary"
        );
    assert_eq!(ctx.prefix_len, Some(2));
    let has_blob = ctx
        .conversation
        .iter()
        .any(|i| {
            matches!(i, ConversationItem::User(u)
                if u.content.iter().any(|p| matches!(p,
                    ContentPart::Text { text } if text.contains("<background_context>"))))
        });
    assert!(
            has_blob,
            "oversize fallback must produce a background_context blob"
        );
}
#[test]
fn verbatim_fork_empty_after_filter_fails_open_to_new() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let items = vec![ConversationItem::user("/goal do the thing")];
    let ctx = verbatim_or_normalize_fork(items, 256_000);
    assert_eq!(ctx.source, InitialContextSource::New);
    assert!(!ctx.verbatim_fork);
    assert!(ctx.conversation.is_empty());
}
#[test]
fn forked_initial_context_system_only_fails_open_to_new() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let ctx = forked_initial_context(vec![ConversationItem::system("sys")]);
    assert_eq!(ctx.source, InitialContextSource::New);
    assert!(!ctx.verbatim_fork);
    assert!(ctx.conversation.is_empty());
    assert!(ctx.copy_error.is_some());
}
#[test]
fn fork_context_normalized_only_for_summarized() {
    assert!(!fork_context_normalized(
            &InitialContextSource::Forked,
            true
        ));
    assert!(fork_context_normalized(
            &InitialContextSource::Forked,
            false
        ));
    assert!(!fork_context_normalized(&InitialContextSource::New, false));
    assert!(!fork_context_normalized(
            &InitialContextSource::Resumed,
            false
        ));
    use xai_grok_sampling_types::conversation::ConversationItem;
    let verbatim = verbatim_or_normalize_fork(
        vec![
                ConversationItem::system("sys"),
                ConversationItem::user("q"),
                ConversationItem::assistant("a"),
            ],
        256_000,
    );
    assert!(verbatim.verbatim_fork);
    assert!(!fork_context_normalized(
            &verbatim.source,
            verbatim.verbatim_fork
        ));
    let summarized = verbatim_or_normalize_fork(
        vec![
                ConversationItem::system("sys"),
                ConversationItem::user("q with text"),
                ConversationItem::assistant("a"),
            ],
        1,
    );
    assert!(!summarized.verbatim_fork);
    assert!(fork_context_normalized(
            &summarized.source,
            summarized.verbatim_fork
        ));
}
fn bootstrap_test_request(fork_context: bool) -> SubagentRequest {
    SubagentRequest {
        id: "bootstrap-test".into(),
        prompt: "plan".into(),
        description: "d".into(),
        subagent_type: "general-purpose".into(),
        parent_session_id: "parent".into(),
        parent_prompt_id: None,
        resume_from: None,
        cwd: None,
        runtime_overrides: Default::default(),
        run_in_background: false,
        surface_completion: false,
        await_to_completion: false,
        fork_context,
        owner: SubagentOwner::Task,
        cancel_token: CancellationToken::new(),
    }
}
#[tokio::test]
async fn bootstrap_no_fork_is_new() {
    let req = bootstrap_test_request(false);
    let ctx = ctx_with_toggle(HashMap::new());
    let child = SessionInfo {
        id: acp::SessionId::new("child-boot"),
        cwd: "/tmp".into(),
    };
    let out = bootstrap_initial_context(
            &req,
            None,
            &ctx,
            &child,
            Path::new("/tmp"),
            "m",
            128_000,
        )
        .await;
    match out {
        BootstrapInitialContext::Ready(ic) => {
            assert_eq!(ic.source, InitialContextSource::New);
            assert!(ic.conversation.is_empty());
            assert!(ic.copy_error.is_none());
        }
        BootstrapInitialContext::ResumeAbort(m) => panic!("unexpected abort: {m}"),
    }
}
#[tokio::test]
async fn bootstrap_fork_without_parent_fails_open() {
    let req = bootstrap_test_request(true);
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.parent_chat_state = None;
    ctx.parent_session_info = None;
    let child = SessionInfo {
        id: acp::SessionId::new("child-boot2"),
        cwd: "/tmp".into(),
    };
    let out = bootstrap_initial_context(
            &req,
            None,
            &ctx,
            &child,
            Path::new("/tmp"),
            "m",
            128_000,
        )
        .await;
    match out {
        BootstrapInitialContext::Ready(ic) => {
            assert_eq!(ic.source, InitialContextSource::New);
            assert!(ic.copy_error.is_some());
        }
        BootstrapInitialContext::ResumeAbort(m) => {
            panic!("fork must fail open, not abort: {m}")
        }
    }
}
#[tokio::test]
async fn bootstrap_fork_live_parent_chat_state_is_forked_with_marker() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    const MARKER: &str = "UNIQUE_LIVE_FORK_MARKER_xyz789";
    let req = bootstrap_test_request(true);
    let mut ctx = ctx_with_toggle(HashMap::new());
    let chat = spawn_test_parent_chat_state("grok-4.5");
    chat.replace_conversation(
        vec![
            ConversationItem::system("parent system"),
            ConversationItem::user(format!("{MARKER} implement multi-repo fix")),
            ConversationItem::assistant("noted the multi-repo work"),
        ],
    );
    ctx.parent_chat_state = Some(chat);
    ctx.parent_session_info = None;
    let child = SessionInfo {
        id: acp::SessionId::new("child-boot-live"),
        cwd: "/tmp".into(),
    };
    let out = bootstrap_initial_context(
            &req,
            None,
            &ctx,
            &child,
            Path::new("/tmp"),
            "m",
            128_000,
        )
        .await;
    match out {
        BootstrapInitialContext::Ready(ic) => {
            assert_eq!(ic.source, InitialContextSource::Forked);
            assert!(ic.copy_error.is_none());
            assert!(
                    ic.verbatim_fork,
                    "small complete-tail parent must mirror verbatim"
                );
            assert_eq!(ic.conversation.len(), 3);
            assert_eq!(ic.prefix_len, Some(3));
            assert!(matches!(ic.conversation[0], ConversationItem::System(_)));
            assert!(matches!(ic.conversation[1], ConversationItem::User(_)));
            assert!(matches!(ic.conversation[2], ConversationItem::Assistant(_)));
            let text: String = ic
                .conversation
                .iter()
                .filter_map(|item| match item {
                    ConversationItem::User(u) => {
                        Some(
                            u
                                .content
                                .iter()
                                .filter_map(|p| match p {
                                    xai_grok_sampling_types::conversation::ContentPart::Text {
                                        text,
                                    } => Some(text.as_ref()),
                                    _ => None,
                                })
                                .collect::<String>(),
                        )
                    }
                    _ => None,
                })
                .collect();
            assert!(
                    text.contains(MARKER),
                    "live parent marker must appear verbatim: {text}"
                );
            assert!(
                    !text.contains("<background_context>"),
                    "verbatim mirror must NOT wrap items in a background_context blob: {text}"
                );
        }
        BootstrapInitialContext::ResumeAbort(m) => panic!("unexpected abort: {m}"),
    }
}
#[tokio::test]
async fn copy_session_data_preserves_parent_chat_history() {
    use crate::sampling::ConversationItem;
    use crate::session::storage::StorageAdapter;
    use crate::session::storage::jsonl::JsonlStorageAdapter;
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    let adapter = JsonlStorageAdapter::with_root(root.to_path_buf());
    let parent_info = SessionInfo {
        id: acp::SessionId::new("parent-fork-test"),
        cwd: "/workspace".to_string(),
    };
    adapter.init_session(&parent_info, acp::ModelId::new("test-model")).await.unwrap();
    adapter
        .append_chat_message(&parent_info, &ConversationItem::user("What files?"))
        .await
        .unwrap();
    adapter
        .append_chat_message(&parent_info, &ConversationItem::assistant("listed"))
        .await
        .unwrap();
    let child_info = SessionInfo {
        id: acp::SessionId::new("child-fork-test"),
        cwd: "/workspace".to_string(),
    };
    let result = adapter
        .copy_session_data_sync(
            &parent_info,
            &child_info,
            crate::session::storage::CopySessionOptions {
                parent_session_id: Some("parent-fork-test".to_string()),
                new_model_id: Some("test-model".to_string()),
                session_kind: Some("subagent_fork".to_string()),
                fork_context_source: Some("forked".to_string()),
                copy_plan_state: false,
                copy_plan_mode_state: false,
                copy_signals: false,
                copy_tool_state: false,
                fork_filter: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(result.chat_messages_copied > 0, "should copy chat history");
    let child_data = adapter.load_session(&child_info).await.unwrap();
    assert_eq!(
            child_data.summary.session_kind.as_deref(),
            Some("subagent_fork")
        );
    assert_eq!(
            child_data.summary.fork_context_source.as_deref(),
            Some("forked")
        );
    assert_eq!(
            child_data.summary.parent_session_id.as_deref(),
            Some("parent-fork-test")
        );
    assert!(
            !child_data.chat_history.is_empty(),
            "child should have inherited parent chat history"
        );
}
fn make_validation_ctx(toggle: HashMap<String, bool>) -> SubagentValidationContext {
    SubagentValidationContext {
        parent_cwd: PathBuf::from("/tmp"),
        subagent_toggle: toggle,
        ..Default::default()
    }
}
#[test]
fn validate_subagent_type_returns_ok_for_known_enabled_agent() {
    let ctx = make_validation_ctx(HashMap::new());
    let outcome = validate_subagent_type("explore", &ctx);
    assert!(
            matches!(outcome, SubagentValidateTypeOutcome::Ok),
            "expected Ok, got {outcome:?}",
        );
}
#[test]
fn validate_subagent_type_returns_unknown_for_invented_type() {
    let ctx = make_validation_ctx(HashMap::new());
    let outcome = validate_subagent_type("totally-invented-agent-name", &ctx);
    match outcome {
        SubagentValidateTypeOutcome::Unknown { available } => {
            for expected in ["general-purpose", "explore", "plan", "oracle", "xdotcom"] {
                assert!(
                        available.iter().any(|n| n == expected),
                        "available list must include built-in {expected:?}: {available:?}",
                    );
            }
            let mut sorted = available.clone();
            sorted.sort();
            assert_eq!(available, sorted, "available must be sorted");
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}
#[test]
fn validate_subagent_type_returns_disabled_when_toggled_off() {
    let toggle = HashMap::from([("explore".to_string(), false)]);
    let ctx = make_validation_ctx(toggle);
    let outcome = validate_subagent_type("explore", &ctx);
    assert!(
            matches!(outcome, SubagentValidateTypeOutcome::Disabled),
            "expected Disabled, got {outcome:?}",
        );
}
#[test]
fn validate_subagent_type_returns_not_allowed_when_outside_allow_list() {
    let mut ctx = make_validation_ctx(HashMap::new());
    ctx.allowed_subagent_types = Some(vec!["plan".to_string()]);
    let outcome = validate_subagent_type("explore", &ctx);
    match outcome {
        SubagentValidateTypeOutcome::NotAllowed { allowed } => {
            assert_eq!(allowed, vec!["plan".to_string()]);
        }
        other => panic!("expected NotAllowed, got {other:?}"),
    }
}
#[test]
fn validate_subagent_type_allow_list_is_case_insensitive() {
    for (requested, allowed) in [
        ("explore", vec!["EXPLORE".to_string()]),
        ("EXPLORE", vec!["explore".to_string()]),
        ("Explore", vec!["eXpLoRe".to_string()]),
        ("explore", vec!["plan".to_string(), "EXPLORE".to_string()]),
    ] {
        let mut ctx = make_validation_ctx(HashMap::new());
        ctx.cli_agent_names = vec![requested.to_string()];
        ctx.allowed_subagent_types = Some(allowed.clone());
        assert!(
                matches!(
                    validate_subagent_type(requested, &ctx),
                    SubagentValidateTypeOutcome::Ok,
                ),
                "{requested:?} should be permitted by allow-list {allowed:?}",
            );
    }
}
#[test]
fn validate_subagent_type_unknown_includes_cli_agents_in_available() {
    let mut ctx = make_validation_ctx(HashMap::new());
    ctx.cli_agent_names = vec!["user-defined-agent".to_string()];
    match validate_subagent_type("invented", &ctx) {
        SubagentValidateTypeOutcome::Unknown { available } => {
            assert!(
                    available.iter().any(|n| n == "user-defined-agent"),
                    "cli agent name missing from available list: {available:?}",
                );
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}
#[test]
fn validate_subagent_type_unknown_dedupes_cli_against_builtins() {
    let mut ctx = make_validation_ctx(HashMap::new());
    ctx.cli_agent_names = vec!["explore".to_string()];
    match validate_subagent_type("invented", &ctx) {
        SubagentValidateTypeOutcome::Unknown { available } => {
            let count = available.iter().filter(|n| n.as_str() == "explore").count();
            assert_eq!(count, 1, "explore must appear once: {available:?}");
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}
#[test]
fn validate_subagent_type_unknown_omits_disabled_types_from_available_list() {
    let toggle = HashMap::from([("explore".to_string(), false)]);
    let ctx = make_validation_ctx(toggle);
    match validate_subagent_type("explor", &ctx) {
        SubagentValidateTypeOutcome::Unknown { available } => {
            assert!(
                    !available.iter().any(|n| n == "explore"),
                    "disabled type must not appear in available: {available:?}",
                );
            assert!(
                    available.iter().any(|n| n == "general-purpose"),
                    "non-disabled built-ins must still appear: {available:?}",
                );
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}
#[test]
fn validate_subagent_type_recognizes_cli_agent_by_name() {
    let mut ctx = make_validation_ctx(HashMap::new());
    ctx.cli_agent_names = vec!["user-defined".to_string()];
    assert!(matches!(
            validate_subagent_type("user-defined", &ctx),
            SubagentValidateTypeOutcome::Ok,
        ));
}
#[test]
fn summarize_tool_config_uses_name_override_and_strips_namespace() {
    use xai_grok_tools::registry::types::{ToolConfig, ToolServerConfig};
    use xai_grok_tools::types::tool::ToolKind;
    let mut read = ToolConfig::from_id("GrokBuild:read_file");
    read.kind = Some(ToolKind::Read);
    let mut read_dup = ToolConfig::from_id("Codex:read_file");
    read_dup.kind = Some(ToolKind::Read);
    read_dup.name_override = Some("codex_read".to_string());
    let mut grep = ToolConfig::from_id("OpenCode:grep");
    grep.kind = Some(ToolKind::Search);
    grep.name_override = Some("alt_grep".to_string());
    let mcp = ToolConfig::from_id("MCP:custom");
    let config = ToolServerConfig {
        tools: vec![read, read_dup, grep, mcp],
        behavior_preset: None,
    };
    let summary = summarize_tool_config(&config);
    assert_eq!(
            summary.tool_names.get(&ToolKind::Read).unwrap(),
            "read_file"
        );
    assert_eq!(
            summary.tool_names.get(&ToolKind::Search).unwrap(),
            "alt_grep"
        );
    assert!(summary.can_read && summary.can_search && !summary.can_execute);
    assert_eq!(summary.tool_names.len(), 2);
}
#[test]
fn describe_subagent_type_unknown_returns_sorted_available() {
    let ctx = ctx_with_toggle(HashMap::new());
    match describe_subagent_type("totally-invented-type", None, &ctx) {
        SubagentDescribeOutcome::Unknown { available } => {
            let mut sorted = available.clone();
            sorted.sort();
            assert_eq!(available, sorted, "available must be sorted");
            assert!(available.iter().any(|n| n == "general-purpose"));
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}
/// Regression: on the DEFAULT grok-build host —
/// the primary `/goal` host — the `general-purpose` toolset's only
/// file-mutator is `search_replace` (`ToolKind::Edit`); the `write`
/// tool (`ToolKind::Write`) is injection-only and absent from the
/// pre-injection describe probe. The planner gate must therefore key on
/// the Edit-class capability, which this asserts is present.
#[test]
fn describe_default_host_general_purpose_has_edit_not_write() {
    use xai_grok_tools::types::tool::ToolKind;
    let ctx = ctx_with_toggle(HashMap::new());
    let SubagentDescribeOutcome::Ok(summary) = describe_subagent_type(
        "general-purpose",
        None,
        &ctx,
    ) else {
        panic!("expected Ok for default-host general-purpose");
    };
    assert!(summary.can_read, "default host reads (read_file)");
    assert!(
            summary.tool_names.contains_key(&ToolKind::Edit),
            "default host's file-mutator is search_replace (Edit): {:?}",
            summary.tool_names,
        );
    assert!(
            !summary.tool_names.contains_key(&ToolKind::Write),
            "the injection-only `write` tool must NOT be in the pre-injection probe",
        );
}
/// Requirement 3 (fail-open trigger): an `agent_type` that does not resolve
/// to a harness `AgentDefinition` reports `Unknown`, which the `/goal`
/// resolver maps to a `ToolsetUnknown` fail-open to the session harness.
#[test]
fn goal_harness_override_unresolvable_returns_unknown() {
    let ctx = ctx_with_toggle(HashMap::new());
    match describe_subagent_type(
        "general-purpose",
        Some("totally-bogus-harness"),
        &ctx,
    ) {
        SubagentDescribeOutcome::Unknown { .. } => {}
        other => {
            panic!("an unresolvable harness override must fail open as Unknown: {other:?}")
        }
    }
}
/// The model fallback only fires for a strict harness: a custom profile
/// running a stock/vision model leaves subagents on the default harness, so
/// they keep native image input.
#[test]
fn subagent_keeps_default_flavor_when_parent_model_is_non_strict() {
    use xai_grok_agent::config::BuiltinAgentName;
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.parent_agent_name = Some("ai-oncall-bot".to_string());
    ctx.parent_model_agent_type = Some(
        BuiltinAgentName::GrokBuildPlan.as_ref().to_string(),
    );
    let mut def = resolve_agent_definition("general-purpose", &ctx).expect("resolves");
    resolve_subagent_toolset("general-purpose", None, &ctx, &mut def);
    assert!(
            !crate::session::is_cursor_user_template(&def.user_message_template),
            "a non-strict parent model must leave subagents on the default harness",
        );
}
fn test_gcs_context(ctx: &SubagentSpawnContext) -> GcsUploadContext {
    GcsUploadContext {
        bucket_url: None,
        upload_method: None,
        model_id: None,
        cwd: None,
        isolation_mode: None,
        capability_mode: None,
        reasoning_effort: None,
        role_name: None,
        parent_prompt_id: None,
        depth: 0,
        auth_manager: ctx.auth_manager.clone(),
    }
}
#[tokio::test]
async fn cancel_pending_shell_child_presents_one_cancelled_finish() {
    let mut ctx = ctx_with_toggle(HashMap::new());
    let (parent_cmd_tx, mut parent_cmd_rx) = mpsc::unbounded_channel();
    ctx.parent_cmd_tx = Some(parent_cmd_tx);
    let (child_cmd_tx, mut child_cmd_rx) = mpsc::unbounded_channel();
    let (gateway, mut gateway_rx) = test_gateway_with_receiver();
    let request = auto_wake_test_request("promote-cancel");
    let meta_dir = tempfile::tempdir().expect("meta dir");
    let result = cancel_pending_shell_child(
            &child_cmd_tx,
            SessionThread::from_handle(std::thread::spawn(|| {})),
            &ctx.workspace_ops,
            &request.id,
            &acp::SessionId::new(request.id.clone()),
            meta_dir.path(),
            None,
            false,
            42,
            &test_gcs_context(&ctx),
            UNPROMOTED_SESSION_THREAD_EXIT_TIMEOUT,
            UnpromotedChildDisposition::Cancelled,
        )
        .await;
    assert!(matches!(child_cmd_rx.try_recv(), Ok(SessionCommand::Cancel(_))));
    assert!(matches!(
            child_cmd_rx.try_recv(),
            Ok(SessionCommand::Shutdown(_))
        ));
    assert!(result.cancelled);
    assert!(!result.success);
    let mut completion_data = ShellCompletionData::from_context(&ctx);
    completion_data.spawned_notification_emitted = true;
    let completion = ChildCompletion {
        request,
        result,
        completion_data,
        disposition: CompletionDisposition {
            foreground_delivered: false,
            backgrounded: false,
            waiter_delivered: false,
            explicitly_killed: false,
            should_surface: false,
        },
    };
    let will_wake = will_wake_for(&completion);
    present_child_completion(completion, &gateway, will_wake);
    let mut persisted = 0;
    while let Ok(command) = parent_cmd_rx.try_recv() {
        if matches!(
                command,
                SessionCommand::XaiSessionNotification {
                    notification: SessionNotification {
                        update: SessionUpdate::SubagentFinished { status, .. },
                        ..
                    }
                } if status == "cancelled"
            ) {
            persisted += 1;
        }
    }
    assert_eq!(persisted, 1);
    let mut live = 0;
    while let Ok(message) = gateway_rx.try_recv() {
        if matches!(
                message,
                xai_acp_lib::AcpClientMessage::ExtNotification(args)
                    if args.request.params.get().contains("\"status\":\"cancelled\"")
            ) {
            live += 1;
        }
    }
    assert_eq!(live, 1);
}
async fn run_promote_cancel_with_worktree(
    worktree: &Path,
    worktree_freshly_created: bool,
) {
    let ctx = ctx_with_toggle(HashMap::new());
    let (child_cmd_tx, mut child_cmd_rx) = mpsc::unbounded_channel();
    let meta_dir = tempfile::tempdir().expect("meta dir");
    let result = cancel_pending_shell_child(
            &child_cmd_tx,
            SessionThread::from_handle(std::thread::spawn(|| {})),
            &ctx.workspace_ops,
            "worktree-cancel",
            &acp::SessionId::new("worktree-cancel"),
            meta_dir.path(),
            Some(worktree),
            worktree_freshly_created,
            42,
            &test_gcs_context(&ctx),
            UNPROMOTED_SESSION_THREAD_EXIT_TIMEOUT,
            UnpromotedChildDisposition::Cancelled,
        )
        .await;
    assert!(matches!(child_cmd_rx.try_recv(), Ok(SessionCommand::Cancel(_))));
    assert!(matches!(
            child_cmd_rx.try_recv(),
            Ok(SessionCommand::Shutdown(_))
        ));
    assert!(result.cancelled);
}
/// A pending cancel removes a freshly-created worktree but preserves a
/// resumed child worktree owned by its source.
#[tokio::test]
async fn cancel_pending_at_promote_removes_fresh_worktree_preserves_resumed() {
    xai_test_utils::require_git!();
    use xai_test_utils::git::{git_commit_all, init_git_repo};
    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    init_git_repo(&repo);
    std::fs::write(repo.join("tracked.txt"), "original").unwrap();
    git_commit_all(&repo, "initial");
    let fresh = temp.path().join("subagent-fresh");
    xai_fast_worktree::WorktreeBuilder::new(&repo, &fresh)
        .standalone(true)
        .create()
        .unwrap();
    assert!(fresh.exists());
    run_promote_cancel_with_worktree(&fresh, true).await;
    assert!(
            !fresh.exists(),
            "freshly-created worktree must be removed on pending-kill"
        );
    let resumed = temp.path().join("subagent-resumed");
    xai_fast_worktree::WorktreeBuilder::new(&repo, &resumed)
        .standalone(true)
        .create()
        .unwrap();
    std::fs::write(resumed.join("tracked.txt"), "source edit").unwrap();
    assert!(resumed.exists());
    run_promote_cancel_with_worktree(&resumed, false).await;
    assert!(
            resumed.exists(),
            "resumed subagent's reused worktree must be preserved (source owns it)"
        );
    assert_eq!(
            std::fs::read_to_string(resumed.join("tracked.txt")).unwrap(),
            "source edit",
            "the source's working state must be left untouched"
        );
}
fn running_meta_json(id: &str) -> String {
    format!(
            r#"{{
                "subagent_id": "{id}",
                "parent_session_id": "test-parent",
                "child_session_id": "{id}",
                "subagent_type": "explore",
                "description": "",
                "prompt": "",
                "status": "running",
                "started_at": "2026-01-01T00:00:00Z"
            }}"#
        )
}
#[tokio::test]
async fn unproven_thread_exit_preserves_fresh_worktree() {
    let ctx = ctx_with_toggle(HashMap::new());
    let (child_cmd_tx, mut child_cmd_rx) = mpsc::unbounded_channel();
    let meta_dir = tempfile::tempdir().expect("meta dir");
    std::fs::write(meta_dir.path().join("meta.json"), running_meta_json("unproven-exit"))
        .expect("write running meta");
    let worktree = tempfile::tempdir().expect("worktree");
    let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
    let thread = SessionThread::from_handle(
        std::thread::spawn(move || {
            let _ = hold_rx.recv();
        }),
    );
    let result = cancel_pending_shell_child(
            &child_cmd_tx,
            thread,
            &ctx.workspace_ops,
            "unproven-exit",
            &acp::SessionId::new("unproven-exit"),
            meta_dir.path(),
            Some(worktree.path()),
            true,
            42,
            &test_gcs_context(&ctx),
            std::time::Duration::ZERO,
            UnpromotedChildDisposition::Cancelled,
        )
        .await;
    assert!(matches!(child_cmd_rx.try_recv(), Ok(SessionCommand::Cancel(_))));
    assert!(matches!(
            child_cmd_rx.try_recv(),
            Ok(SessionCommand::Shutdown(_))
        ));
    assert!(result.cancelled);
    assert!(
            worktree.path().exists(),
            "worktree must stay when actor exit is not proven"
        );
    let meta: SubagentMeta = serde_json::from_str(
            &std::fs::read_to_string(meta_dir.path().join("meta.json"))
                .expect("read meta"),
        )
        .expect("parse meta");
    assert_eq!(meta.status, "cancelled");
    assert!(meta.completed_at.is_some());
    assert_eq!(meta.error.as_deref(), Some("Subagent was cancelled"));
    drop(hold_tx);
}
#[tokio::test]
async fn startup_admission_timeout_is_failed_not_cancelled() {
    let mut ctx = ctx_with_toggle(HashMap::new());
    let (parent_cmd_tx, mut parent_cmd_rx) = mpsc::unbounded_channel();
    ctx.parent_cmd_tx = Some(parent_cmd_tx);
    let (child_cmd_tx, mut child_cmd_rx) = mpsc::unbounded_channel();
    let (gateway, mut gateway_rx) = test_gateway_with_receiver();
    let request = auto_wake_test_request("promote-timeout");
    let meta_dir = tempfile::tempdir().expect("meta dir");
    std::fs::write(meta_dir.path().join("meta.json"), running_meta_json(&request.id))
        .expect("write running meta");
    let result = cancel_pending_shell_child(
            &child_cmd_tx,
            SessionThread::from_handle(std::thread::spawn(|| {})),
            &ctx.workspace_ops,
            &request.id,
            &acp::SessionId::new(request.id.clone()),
            meta_dir.path(),
            None,
            false,
            42,
            &test_gcs_context(&ctx),
            UNPROMOTED_SESSION_THREAD_EXIT_TIMEOUT,
            UnpromotedChildDisposition::AdmissionTimedOut,
        )
        .await;
    assert!(matches!(child_cmd_rx.try_recv(), Ok(SessionCommand::Cancel(_))));
    assert!(matches!(
            child_cmd_rx.try_recv(),
            Ok(SessionCommand::Shutdown(_))
        ));
    assert!(!result.cancelled);
    assert!(!result.success);
    assert_eq!(result.status(), "failed");
    assert_eq!(
            result.error.as_deref(),
            Some("Subagent initial prompt was not admitted before the deadline")
        );
    let meta: SubagentMeta = serde_json::from_str(
            &std::fs::read_to_string(meta_dir.path().join("meta.json"))
                .expect("read meta"),
        )
        .expect("parse meta");
    assert_eq!(meta.status, "failed");
    let mut completion_data = ShellCompletionData::from_context(&ctx);
    completion_data.spawned_notification_emitted = true;
    let completion = ChildCompletion {
        request,
        result,
        completion_data,
        disposition: CompletionDisposition {
            foreground_delivered: false,
            backgrounded: false,
            waiter_delivered: false,
            explicitly_killed: false,
            should_surface: false,
        },
    };
    let will_wake = will_wake_for(&completion);
    present_child_completion(completion, &gateway, will_wake);
    let mut persisted = 0;
    while let Ok(command) = parent_cmd_rx.try_recv() {
        if matches!(
                command,
                SessionCommand::XaiSessionNotification {
                    notification: SessionNotification {
                        update: SessionUpdate::SubagentFinished { status, .. },
                        ..
                    }
                } if status == "failed"
            ) {
            persisted += 1;
        }
    }
    assert_eq!(persisted, 1);
    let mut live = 0;
    while let Ok(message) = gateway_rx.try_recv() {
        if matches!(
                message,
                xai_acp_lib::AcpClientMessage::ExtNotification(args)
                    if args.request.params.get().contains("\"status\":\"failed\"")
            ) {
            live += 1;
        }
    }
    assert_eq!(live, 1);
}
fn test_model_entry(model_id: &str) -> crate::agent::config::ModelEntry {
    crate::agent::config::ModelEntry {
        info: crate::agent::config::ModelInfo {
            user_selectable: true,
            id: None,
            model_family: None,
            model: model_id.to_string(),
            base_url: String::new(),
            name: None,
            description: None,
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            api_backend: Default::default(),
            request_compat: None,
            endpoint_path: None,
            auth_scheme: Default::default(),
            extra_headers: Default::default(),
            query_params: Default::default(),
            env_http_headers: Default::default(),
            context_window: std::num::NonZeroU64::new(256_000).unwrap(),
            auto_compact_threshold_percent: None,
            system_prompt_label: None,
            use_concise: false,
            agent_type: crate::agent::config::default_agent_type(),
            inference_idle_timeout_secs: None,
            max_retries: None,
            subagent_rate_limit_max_attempts: None,
            hidden: false,
            supported_in_api: true,
            reasoning_effort: None,
            supports_reasoning_effort: false,
            reasoning_efforts: Vec::new(),
            supports_backend_search: false,
            compactions_remaining: None,
            compaction_at_tokens: None,
            show_model_fingerprint: false,
            stream_tool_calls: None,
            laziness_detector: crate::agent::config::LazinessDetectorPerModelConfig::default(),
        },
        api_key: None,
        env_key: None,
        auth_provider: None,
        platform_oauth_active: false,
        api_base_url: None,
    }
}
fn byok_model_entry(model_id: &str) -> crate::agent::config::ModelEntry {
    crate::agent::config::ModelEntry {
        api_key: Some("byok-key".to_string()),
        ..test_model_entry(model_id)
    }
}
#[test]
fn subagent_auth_type_rule() {
    use crate::agent::auth_method::{CACHED_TOKEN_AUTH_METHOD_ID, XAI_API_KEY_METHOD_ID};
    use xai_chat_state::AuthType;
    let session = acp::AuthMethodId::new(CACHED_TOKEN_AUTH_METHOD_ID);
    let api_key = acp::AuthMethodId::new(XAI_API_KEY_METHOD_ID);
    let byok = byok_model_entry("grok-byok");
    let plain = test_model_entry("grok-plain");
    assert_eq!(
            super::subagent_auth_type(Some(&byok), &session),
            AuthType::ApiKey
        );
    assert_eq!(
            super::subagent_auth_type(Some(&byok), &api_key),
            AuthType::ApiKey
        );
    assert_eq!(
            super::subagent_auth_type(Some(&plain), &session),
            AuthType::SessionToken,
        );
    assert_eq!(
            super::subagent_auth_type(Some(&plain), &api_key),
            AuthType::ApiKey
        );
    assert_eq!(
            super::subagent_auth_type(None, &session),
            AuthType::SessionToken
        );
    assert_eq!(super::subagent_auth_type(None, &api_key), AuthType::ApiKey);
}
#[test]
fn fresh_tool_model_accepts_visible_key_and_internal_id() {
    let mut models = indexmap::IndexMap::new();
    models.insert("grok-3".to_string(), test_model_entry("grok-3-2025-02-15"));
    assert!(
            super::handle_request::task_model_override_error(
                Some("grok-3"),
                ModelOverrideProvenance::Tool,
                false,
                &models,
                false,
            )
            .is_none(),
            "key lookup should succeed"
        );
    assert!(
            super::handle_request::task_model_override_error(
                Some("grok-3-2025-02-15"),
                ModelOverrideProvenance::Tool,
                false,
                &models,
                false,
            )
            .is_none(),
            "info().model lookup should succeed"
        );
}
#[test]
fn fresh_tool_model_rejects_unavailable_exact_key_over_visible_slug_collision() {
    let mut models = indexmap::IndexMap::new();
    models.insert("visible-alias".to_string(), test_model_entry("collision"));
    let mut unavailable_exact = test_model_entry("hidden-internal");
    unavailable_exact.info.hidden = true;
    models.insert("collision".to_string(), unavailable_exact);
    assert_eq!(
            super::handle_request::task_model_override_error(
                Some("collision"),
                ModelOverrideProvenance::Tool,
                false,
                &models,
                false,
            )
            .as_deref(),
            Some(
                "Unknown Task.model slug 'collision'. Valid model slugs: visible-alias. \
                 Omit `model` to inherit the parent model."
            ),
            "validation must inspect the unavailable exact-key entry selected by execution"
        );
}
#[test]
fn fresh_tool_model_rejects_unavailable_first_slug_collision() {
    let mut models = indexmap::IndexMap::new();
    let mut unavailable_first = test_model_entry("shared-routing-slug");
    unavailable_first.info.user_selectable = false;
    models.insert("blocked-first".to_string(), unavailable_first);
    models.insert("visible-second".to_string(), test_model_entry("shared-routing-slug"));
    assert_eq!(
            super::handle_request::task_model_override_error(
                Some("shared-routing-slug"),
                ModelOverrideProvenance::Tool,
                false,
                &models,
                false,
            )
            .as_deref(),
            Some(
                "Unknown Task.model slug 'shared-routing-slug'. Valid model slugs: \
                 visible-second. Omit `model` to inherit the parent model."
            ),
            "validation must inspect the first routing-slug entry selected by execution"
        );
}
#[test]
fn fresh_tool_model_rejects_unknown_and_nonavailable_entries() {
    let mut models = indexmap::IndexMap::new();
    models.insert("zeta".to_string(), test_model_entry("zeta-internal"));
    let mut hidden = test_model_entry("hidden-internal");
    hidden.info.hidden = true;
    models.insert("hidden".to_string(), hidden);
    let mut not_selectable = test_model_entry("disabled-internal");
    not_selectable.info.user_selectable = false;
    models.insert("disabled".to_string(), not_selectable);
    let mut oauth_only = test_model_entry("oauth-only-internal");
    oauth_only.info.supported_in_api = false;
    models.insert("oauth-only".to_string(), oauth_only);
    models.insert("alpha".to_string(), test_model_entry("alpha-internal"));
    for requested in [
        "stale-model",
        "hidden",
        "hidden-internal",
        "disabled",
        "disabled-internal",
        "oauth-only",
        "oauth-only-internal",
    ] {
        let error = super::handle_request::task_model_override_error(
                Some(requested),
                ModelOverrideProvenance::Tool,
                false,
                &models,
                false,
            )
            .unwrap();
        assert_eq!(
                error,
                format!(
                    "Unknown Task.model slug '{requested}'. Valid model slugs: alpha, zeta. \
                     Omit `model` to inherit the parent model."
                )
            );
        assert!(!error.contains("grok models"));
    }
    assert!(
            super::handle_request::task_model_override_error(
                Some("oauth-only"),
                ModelOverrideProvenance::Tool,
                false,
                &models,
                true,
            )
            .is_none(),
            "OAuth-only model should resolve for session auth"
        );
}
#[test]
fn resumed_tool_model_override_is_ignored() {
    let empty = indexmap::IndexMap::new();
    assert!(
            super::handle_request::task_model_override_error(
                Some("stale-model"),
                ModelOverrideProvenance::Tool,
                true,
                &empty,
                false,
            )
            .is_none(),
            "resume must preserve source-model pinning"
        );
}
#[test]
fn harness_model_override_keeps_internal_fallback_behavior() {
    let empty = indexmap::IndexMap::new();
    assert!(
            super::handle_request::task_model_override_error(
                Some("internal-model"),
                ModelOverrideProvenance::Harness,
                false,
                &empty,
                false,
            )
            .is_none(),
            "internal role/config pins must retain downstream soft fallback"
        );
}
#[test]
fn normalize_forked_context_empty_parent() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let items = vec![ConversationItem::system("sys prompt")];
    let (conv, prefix_len) = xai_grok_subagent_resolution::context::normalize_forked_context(
        items,
    );
    assert_eq!(conv.len(), 1);
    assert_eq!(prefix_len, 1);
    assert!(matches!(conv[0], ConversationItem::System(_)));
}
fn test_sampling_config(model_slug: &str) -> xai_grok_sampling_types::SamplingConfig {
    use std::num::NonZeroU64;
    xai_grok_sampling_types::SamplingConfig {
        base_url: "https://api.test/v1".to_string(),
        model: model_slug.to_string(),
        max_completion_tokens: None,
        temperature: None,
        top_p: None,
        api_backend: Default::default(),
        adapter_kind: Default::default(),
        request_compat: None,
        endpoint_path: None,
        extra_headers: Default::default(),
        query_params: Default::default(),
        env_http_headers: Default::default(),
        context_window: NonZeroU64::new(256_000).expect("non-zero context window"),
        reasoning_effort: None,
        stream_tool_calls: None,
    }
}
fn spawn_test_parent_chat_state(model_slug: &str) -> xai_chat_state::ChatStateHandle {
    spawn_test_parent_chat_state_at(model_slug, "https://api.test/v1")
}
fn spawn_test_parent_chat_state_at(
    model_slug: &str,
    base_url: &str,
) -> xai_chat_state::ChatStateHandle {
    let (mock, _persistence_rx) = xai_chat_state::MockChatPersistence::new();
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let token = tokio_util::sync::CancellationToken::new();
    let mut sampling_config = test_sampling_config(model_slug);
    sampling_config.base_url = base_url.to_owned();
    xai_chat_state::ChatStateActor::spawn(
        vec![],
        sampling_config,
        Box::new(mock),
        event_tx,
        token,
    )
}
mod rest;
#[tokio::test]
async fn join_worker_task_resumes_worker_panics() {
    let inner = super::worker_runtime()
        .expect("worker runtime")
        .spawn(async { panic!("worker boom") });
    let err = tokio::spawn(join_worker_task::<()>(inner))
        .await
        .expect_err("panic must propagate out of join_worker_task");
    assert!(err.is_panic());
}
#[tokio::test]
async fn join_worker_task_drop_aborts_worker() {
    struct SendOnDrop(Option<tokio::sync::oneshot::Sender<()>>);
    impl Drop for SendOnDrop {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }
    let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let inner = super::worker_runtime()
        .expect("worker runtime")
        .spawn(async move {
            let _probe = SendOnDrop(Some(dropped_tx));
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        });
    started_rx.await.expect("worker started");
    let mut fut = Box::pin(join_worker_task::<()>(inner));
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(
            std::future::Future::poll(fut.as_mut(), &mut cx).is_pending(),
            "worker is pending until aborted"
        );
    drop(fut);
    tokio::time::timeout(std::time::Duration::from_secs(5), dropped_rx)
        .await
        .expect("abort must reach the worker task")
        .expect("drop probe fires on abort");
}
