use super::*;
use std::io::Write as _;

fn spec(name: &str, path: PathBuf, caps: Vec<Capability>) -> SchemeSpec {
    SchemeSpec {
        name: name.into(),
        scheme_path: path,
        capabilities: caps,
        trusted: true,
        gate_fail: None,
        plugin_data_dir: None,
    }
}

fn write_plugin(dir: &std::path::Path, file: &str, source: &str) -> PathBuf {
    let path = dir.join(file);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(source.as_bytes()).unwrap();
    path
}

/// Deterministically-unavailable runtime config (no PATH discovery).
fn offline_config(state_dir: PathBuf) -> SchemeRuntimeConfig {
    SchemeRuntimeConfig {
        state_dir,
        prebuilt_candidates: Vec::new(),
        allow_path_discovery: false,
    }
}

#[tokio::test]
async fn empty_runtime_is_inert() {
    let dir = tempfile::tempdir().unwrap();
    let rt = SchemeRuntime::new(offline_config(dir.path().into()));
    assert!(rt.is_empty());
    assert!(!rt.has_capability(Capability::PreToolGate));

    let d = rt
        .dispatch_pre_tool_use(&PreToolIn {
            tool_name: "shell".into(),
            tool_input_json: "{}".into(),
        })
        .await;
    assert_eq!(d.decision, PreToolOut::Allow);
    assert!(d.results.is_empty());

    let s = rt.dispatch_stop(&StopIn { stop_hook_active: false }).await;
    assert_eq!(s.decision, StopOut::Continue);
    assert!(rt.dispatch_session_start().await.is_empty());
}

#[tokio::test]
async fn missing_toolchain_fails_open_and_logs_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_plugin(dir.path(), "p.ss", "(register-handler! 'pre-tool-use (lambda (ctx) '(deny \"x\")))");
    let rt = SchemeRuntime::new(offline_config(dir.path().into()));
    rt.rebuild_from_specs(vec![spec("p", path, vec![Capability::PreToolGate])])
        .await;
    assert_eq!(rt.len(), 1);
    assert!(rt.has_capability(Capability::PreToolGate));

    // No image can boot: gate must fail open (deny would require the image).
    let d = rt
        .dispatch_pre_tool_use(&PreToolIn {
            tool_name: "shell".into(),
            tool_input_json: "{}".into(),
        })
        .await;
    assert_eq!(d.decision, PreToolOut::Allow);
    assert!(matches!(
        d.results.as_slice(),
        [SchemeCallResult::Unavailable { .. }]
    ));

    let status = rt.live_status().await;
    assert!(!status.image_running);
    assert!(rt.live_eval("(+ 1 2)").await.is_err());
}

#[tokio::test]
async fn untrusted_and_unreadable_specs_are_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let good = write_plugin(dir.path(), "good.ss", "(define x 1)");
    let rt = SchemeRuntime::new(offline_config(dir.path().into()));
    rt.rebuild_from_specs(vec![
        SchemeSpec {
            trusted: false,
            ..spec("untrusted", good.clone(), vec![])
        },
        spec("missing", dir.path().join("nope.ss"), vec![]),
        spec("good", good, vec![Capability::StopGate]),
    ])
    .await;
    assert_eq!(rt.names(), vec!["good".to_string()]);
    assert_eq!(rt.metrics().loads_failed, 2);
}

#[test]
fn gate_fail_mapping() {
    let deny = CallOutcome::Reply(Sexp::parse("(deny \"nope\")").unwrap());
    let allow = CallOutcome::Reply(Sexp::parse("(allow)").unwrap());
    let no_handler = CallOutcome::Reply(Sexp::parse("(no-handler)").unwrap());
    let handler_err = CallOutcome::Reply(Sexp::parse("(err \"boom\")").unwrap());
    let timeout = CallOutcome::Timeout(Duration::from_secs(2));
    let failed = CallOutcome::Failed("io".into());
    let unavailable = CallOutcome::Unavailable;

    // Explicit deny always denies, with the guest reason.
    assert_eq!(
        gate_deny_reason(&deny, GateFailMode::Open, "p", "t").as_deref(),
        Some("nope")
    );
    // Allow / no-handler never deny.
    assert!(gate_deny_reason(&allow, GateFailMode::Closed, "p", "t").is_none());
    assert!(gate_deny_reason(&no_handler, GateFailMode::Closed, "p", "t").is_none());
    // Errors and timeouts obey gate_fail.
    assert!(gate_deny_reason(&handler_err, GateFailMode::Open, "p", "t").is_none());
    assert!(gate_deny_reason(&handler_err, GateFailMode::Closed, "p", "t").is_some());
    assert!(gate_deny_reason(&timeout, GateFailMode::Open, "p", "t").is_none());
    assert!(gate_deny_reason(&timeout, GateFailMode::Closed, "p", "t").is_some());
    assert!(gate_deny_reason(&failed, GateFailMode::Closed, "p", "t").is_some());
    // Whole-feature unavailability never gates.
    assert!(gate_deny_reason(&unavailable, GateFailMode::Closed, "p", "t").is_none());

    // Stop mirror.
    let block = CallOutcome::Reply(Sexp::parse("(block \"more work\")").unwrap());
    let cont = CallOutcome::Reply(Sexp::parse("(continue)").unwrap());
    assert_eq!(
        stop_block_reason(&block, GateFailMode::Open, "p").as_deref(),
        Some("more work")
    );
    assert!(stop_block_reason(&cont, GateFailMode::Closed, "p").is_none());
    assert!(stop_block_reason(&CallOutcome::Timeout(Duration::from_secs(2)), GateFailMode::Closed, "p").is_some());
    assert!(stop_block_reason(&CallOutcome::Unavailable, GateFailMode::Closed, "p").is_none());
}

#[tokio::test]
async fn journal_live_flow_without_image() {
    // Journal-side behavior of redefine/discard works even when the image
    // cannot boot (entries are durable; apply is best-effort).
    let dir = tempfile::tempdir().unwrap();
    let path = write_plugin(dir.path(), "p.ss", "(define x 1)");
    let rt = SchemeRuntime::new(offline_config(dir.path().into()));
    rt.rebuild_from_specs(vec![spec("p", path, vec![Capability::PreToolGate])])
        .await;

    // Unknown plugin / bad event validated before journaling.
    assert!(matches!(
        rt.live_redefine("nope", "pre-tool-use", "(lambda (ctx) '(allow))").await,
        Err(LiveError::NoSuchPlugin(_))
    ));
    assert!(matches!(
        rt.live_redefine("p", "bad-event", "(lambda (ctx) '(allow))").await,
        Err(LiveError::BadEvent(_))
    ));

    // Valid redefine journals first, then fails on the unavailable image.
    assert!(matches!(
        rt.live_redefine("p", "pre-tool-use", "(lambda (ctx) '(allow))").await,
        Err(LiveError::Unavailable)
    ));
    assert_eq!(rt.live_status().await.journal.pending, 1);

    // Discard quarantines the pending entry.
    let status = rt.live_discard().await.unwrap();
    assert_eq!(status.pending, 0);
    assert_eq!(status.committed, 0);
}

// ---------------------------------------------------------------------------
// End-to-end against a real Gambit/Gerbil toolchain. Skipped when absent.
// ---------------------------------------------------------------------------

fn toolchain_present() -> bool {
    which::which("gxi").is_ok() || which::which("gsi").is_ok()
}

const E2E_PLUGIN: &str = r#"
(register-handler! 'pre-tool-use
  (lambda (ctx)
    (let ((input (ctx-ref ctx 'tool-input)))
      (if (and input (string-contains? input "rm -rf"))
          '(deny "dangerous command")
          '(allow)))))

(register-handler! 'before-agent-start
  (lambda (ctx)
    (list 'inject "remember the project style guide" #f)))

(register-handler! 'stop
  (lambda (ctx)
    (if (ctx-ref ctx 'stop-hook-active)
        '(continue)
        '(continue))))
"#;

#[tokio::test]
async fn e2e_real_image_flow() {
    if !toolchain_present() {
        eprintln!("skipping e2e_real_image_flow: no gxi/gsi on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = write_plugin(dir.path(), "guard.ss", E2E_PLUGIN);
    let rt = SchemeRuntime::new(SchemeRuntimeConfig::new(dir.path().into()));
    rt.rebuild_from_specs(vec![spec(
        "guard",
        path,
        vec![
            Capability::PreToolGate,
            Capability::BeforeAgentInject,
            Capability::StopGate,
        ],
    )])
    .await;

    // Gate: deny on dangerous input, allow otherwise.
    let d = rt
        .dispatch_pre_tool_use(&PreToolIn {
            tool_name: "shell".into(),
            tool_input_json: r#"{"command":"rm -rf /"}"#.into(),
        })
        .await;
    assert_eq!(
        d.decision,
        PreToolOut::Deny {
            reason: "dangerous command".into()
        }
    );
    let d = rt
        .dispatch_pre_tool_use(&PreToolIn {
            tool_name: "shell".into(),
            tool_input_json: r#"{"command":"ls"}"#.into(),
        })
        .await;
    assert_eq!(d.decision, PreToolOut::Allow);

    // Inject with per-plugin tag.
    let inj = rt
        .dispatch_before_agent_start(&BeforeAgentStartIn {
            prompt: "hello".into(),
        })
        .await;
    assert_eq!(
        inj.out.inject_context.as_deref(),
        Some("[scheme:guard] remember the project style guide")
    );

    // Stop continues.
    let s = rt.dispatch_stop(&StopIn { stop_hook_active: false }).await;
    assert_eq!(s.decision, StopOut::Continue);

    // Observe events are tolerated with no handlers registered.
    assert!(rt.dispatch_session_start().await.is_empty());
    assert!(
        rt.dispatch_pre_compact(&PreCompactIn { reason: "manual".into() })
            .await
            .is_empty()
    );

    // Self-modification: redefine the gate to always allow, then commit.
    rt.live_redefine("guard", "pre-tool-use", "(lambda (ctx) '(allow))")
        .await
        .unwrap();
    let d = rt
        .dispatch_pre_tool_use(&PreToolIn {
            tool_name: "shell".into(),
            tool_input_json: r#"{"command":"rm -rf /"}"#.into(),
        })
        .await;
    assert_eq!(d.decision, PreToolOut::Allow, "redefined handler must win");
    let status = rt.live_commit().await.unwrap();
    assert_eq!(status.pending, 0);
    assert_eq!(status.committed, 1);

    // eval works and image reports running.
    assert_eq!(rt.live_eval("(+ 1 2)").await.unwrap(), "3");
    let st = rt.live_status().await;
    assert!(st.image_running);
    assert_eq!(st.plugins, vec![("guard".to_string(), false)]);

    // Kill/replay: shoot the image, next dispatch respawns and the committed
    // redefine (always-allow) survives replay.
    rt.live_eval("(exit 9)").await.err(); // image dies mid-call
    let d = rt
        .dispatch_pre_tool_use(&PreToolIn {
            tool_name: "shell".into(),
            tool_input_json: r#"{"command":"rm -rf /"}"#.into(),
        })
        .await;
    assert_eq!(d.decision, PreToolOut::Allow, "committed redefine must survive respawn");

    rt.shutdown_async().await;
}

const E2E_CMD_TOOL_PLUGIN: &str = r#"
(register-command! "greet" "Say hello"
  (lambda (args) (string-append "hello " args)))

(register-tool! "adder" "Adds two numbers" "{\"type\":\"object\",\"properties\":{}}"
  (lambda (input) (string-append "sum for " input)))

(register-handler! 'user-prompt-submit
  (lambda (ctx) '(ok)))
"#;

#[tokio::test]
async fn e2e_registered_commands_and_tools() {
    if !toolchain_present() {
        eprintln!("skipping e2e_registered_commands_and_tools: no gxi/gsi on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = write_plugin(dir.path(), "ext.ss", E2E_CMD_TOOL_PLUGIN);
    let rt = SchemeRuntime::new(SchemeRuntimeConfig::new(dir.path().into()));
    rt.rebuild_from_specs(vec![spec(
        "ext",
        path,
        vec![Capability::RegisterCommand, Capability::RegisterTool],
    )])
    .await;

    let cmds = rt.collect_registered_commands().await;
    assert_eq!(cmds.len(), 1);
    assert_eq!(cmds[0].extension, "ext");
    assert_eq!(cmds[0].name, "greet");
    assert_eq!(cmds[0].description, "Say hello");
    let out = rt
        .invoke_registered_command("ext", "greet", "world")
        .await
        .unwrap();
    assert_eq!(out, "hello world");
    // Unknown command errors cleanly.
    assert!(rt.invoke_registered_command("ext", "nope", "").await.is_err());

    let tools = rt.collect_registered_tools().await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "adder");
    assert!(tools[0].input_schema_json.contains("object"));
    let out = rt
        .invoke_registered_tool("ext", "adder", r#"{"a":1}"#)
        .await
        .unwrap();
    assert_eq!(out, r#"sum for {"a":1}"#);

    // New observe events dispatch without errors (handler present for
    // user-prompt-submit; none for notification/subagent-stop).
    let results = rt.dispatch_user_prompt_submit("build the feature").await;
    assert!(
        results
            .iter()
            .all(|r| matches!(r, SchemeCallResult::Ok { .. })),
        "user_prompt_submit results: {results:?}"
    );
    assert!(rt.dispatch_notification("permission_prompt: ok").await.is_empty());
    assert!(rt.dispatch_subagent_stop("researcher").await.is_empty());

    rt.shutdown_async().await;
}

#[tokio::test]
async fn registered_surface_requires_capability() {
    if !toolchain_present() {
        eprintln!("skipping registered_surface_requires_capability: no gxi/gsi on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = write_plugin(dir.path(), "ext.ss", E2E_CMD_TOOL_PLUGIN);
    let rt = SchemeRuntime::new(SchemeRuntimeConfig::new(dir.path().into()));
    // No RegisterCommand / RegisterTool capabilities declared.
    rt.rebuild_from_specs(vec![spec("ext", path, vec![Capability::PreToolGate])])
        .await;
    assert!(rt.collect_registered_commands().await.is_empty());
    assert!(rt.collect_registered_tools().await.is_empty());
    assert!(
        rt.invoke_registered_command("ext", "greet", "x").await.is_err(),
        "invoke must be capability-gated"
    );
    rt.shutdown_async().await;
}

#[tokio::test]
async fn e2e_bad_pending_redefine_is_quarantined_by_commit() {
    if !toolchain_present() {
        eprintln!("skipping e2e_bad_pending: no gxi/gsi on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = write_plugin(dir.path(), "p.ss", "(register-handler! 'stop (lambda (ctx) '(continue)))");
    let rt = SchemeRuntime::new(SchemeRuntimeConfig::new(dir.path().into()));
    rt.rebuild_from_specs(vec![spec("p", path, vec![Capability::StopGate])])
        .await;

    // A redefine whose source is not a procedure: journaled pending, image rejects.
    let err = rt.live_redefine("p", "stop", "42").await.unwrap_err();
    assert!(matches!(err, LiveError::Image(_)), "got {err:?}");
    assert_eq!(rt.live_status().await.journal.pending, 1);

    // Commit probe must reject and quarantine it.
    let err = rt.live_commit().await.unwrap_err();
    assert!(matches!(err, LiveError::CommitRejected(_)), "got {err:?}");
    let st = rt.live_status().await;
    assert_eq!(st.journal.pending, 0);
    assert_eq!(st.journal.committed, 0);

    rt.shutdown_async().await;
}
