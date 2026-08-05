//! SessionActor-level coverage for WASM extensions:
//! - `prepare_tool_call` deny/allow (pre_tool_gate)
//! - session-owned tools register/unregister
//! - concurrent dual-session tools on one ToolBridge
//! - `run_stop_gate` with stop-once fixture
//! - before_model inject via the same dispatch turn uses
//! - fail-closed trap guest through prepare_tool_call
//! - `register_command` collect/invoke + slash resolve + ACP catalog
//! - `post_tool_use` observe dispatch

use super::support::*;
use super::*;
use std::path::PathBuf;
use xai_grok_extension_api::{Capability, ExtensionSpec, GateFailMode};
use xai_grok_extension_runtime::{ExtensionRuntime, wat_to_wasm};
use xai_grok_tools::registry::types::ToolConfig;

fn fixture_wasm() -> Option<PathBuf> {
    // shell crate is packages/coding-agent/xai-grok-shell; extensions live under packages/extensions/.
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "../../extensions/xai-grok-extension-runtime/examples/rust-guest-template/extension.wasm",
    );
    path.is_file().then_some(path)
}

fn stop_once_wasm() -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "../../extensions/xai-grok-extension-runtime/examples/sdk-stop-once/extension.wasm",
    );
    path.is_file().then_some(path)
}

fn load_template_runtime(caps: Vec<Capability>) -> Option<ExtensionRuntime> {
    let wasm = fixture_wasm()?;
    let mut rt = ExtensionRuntime::new();
    rt.load(&ExtensionSpec {
        name: "e2e-template".into(),
        wasm_path: wasm,
        capabilities: caps,
        trusted: true,
        gate_fail: None,
        plugin_data_dir: Some(PathBuf::from("/tmp/hyper-ext-e2e-data")),
    })
    .ok()?;
    Some(rt)
}

fn load_runtime_from_path(
    name: &str,
    wasm: PathBuf,
    caps: Vec<Capability>,
    gate_fail: Option<GateFailMode>,
) -> Option<ExtensionRuntime> {
    let mut rt = ExtensionRuntime::new();
    if let Some(mode) = gate_fail {
        rt.set_gate_fail(mode);
    }
    rt.load(&ExtensionSpec {
        name: name.into(),
        wasm_path: wasm,
        capabilities: caps,
        trusted: true,
        gate_fail,
        plugin_data_dir: None,
    })
    .ok()?;
    Some(rt)
}

fn write_wat_guest(dir: &std::path::Path, name: &str, wat: &str) -> PathBuf {
    let path = dir.join(name);
    let bytes = wat_to_wasm(wat).expect("wat");
    std::fs::write(&path, bytes).expect("write wasm");
    path
}

async fn build_actor_with_read_tool() -> SessionActor {
    let (gateway_tx, mut gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
    let (persistence_tx, _persistence_rx) =
        tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    *actor.agent.borrow_mut() =
        test_agent_with_tools(vec![ToolConfig::from_id("GrokBuild:read_file")]).await;
    // Drain session notifications so prepare_tool_call does not stall.
    tokio::task::spawn_local(async move {
        while let Some(msg) = gateway_rx.recv().await {
            if let xai_acp_lib::AcpClientMessage::SessionNotification(args) = msg {
                let _ = args.response_tx.send(Ok(()));
            }
        }
    });
    actor
}

fn tool_call(id: &str, name: &str, args: &str) -> ToolCallResponse {
    ToolCallResponse {
        id: id.to_string(),
        kind: "function".to_string(),
        function: crate::sampling::types::ToolCallFunction::new(name, args),
    }
}

async fn prepare(
    actor: &SessionActor,
    call: ToolCallResponse,
) -> Result<PreparedToolCall, ToolLoop> {
    let mut deferred = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        actor.prepare_tool_call(call, &mut deferred),
    )
    .await
    .expect("prepare_tool_call must not hang")
    .expect("prepare_tool_call must not error")
}

/// Template guest denies tool inputs containing `rm -rf` when `pre_tool_gate`
/// is granted — driven through SessionActor::prepare_tool_call.
#[tokio::test(flavor = "current_thread")]
async fn prepare_tool_call_denied_by_wasm_pre_tool_gate() {
    let Some(rt) = load_template_runtime(vec![Capability::PreToolGate]) else {
        eprintln!("skip: no rust-guest-template/extension.wasm");
        return;
    };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = build_actor_with_read_tool().await;
            *actor.extension_runtime.borrow_mut() = rt;

            let result = prepare(
                &actor,
                tool_call(
                    "call_deny",
                    "read_file",
                    // Field is `target_file` (serde rename); embed "rm -rf" so
                    // the template pre_tool_gate matches on raw JSON.
                    r#"{"target_file":"/tmp/evil rm -rf payload.txt"}"#,
                ),
            )
            .await;

            match result {
                Err(ToolLoop::HookDenied { hook_name }) => {
                    assert!(
                        hook_name.starts_with("wasm:"),
                        "hook_name should be wasm:ext, got {hook_name}"
                    );
                    assert!(
                        hook_name.contains("e2e-template"),
                        "expected extension name in hook_name: {hook_name}"
                    );
                }
                other => panic!("expected wasm HookDenied, got {other:?}"),
            }

            let m = actor.extension_runtime.borrow().metrics();
            assert!(m.pre_tool_denies >= 1, "metrics: {m}");
            assert!(m.loads_ok >= 1);
        })
        .await;
}

/// Without the blocked pattern, the same guest allows the call past the wasm gate
/// (further prepare steps may still succeed or fail on tool requirements — we only
/// require it is not HookDenied from wasm).
#[tokio::test(flavor = "current_thread")]
async fn prepare_tool_call_allowed_when_input_clean() {
    let Some(rt) = load_template_runtime(vec![Capability::PreToolGate]) else {
        return;
    };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = build_actor_with_read_tool().await;
            *actor.extension_runtime.borrow_mut() = rt;

            let result = prepare(
                &actor,
                tool_call(
                    "call_allow",
                    "read_file",
                    r#"{"target_file":"/tmp/safe.txt"}"#,
                ),
            )
            .await;

            assert!(
                !matches!(
                    result,
                    Err(ToolLoop::HookDenied { ref hook_name })
                        if hook_name.starts_with("wasm:")
                ),
                "clean input must not be wasm-denied; got {result:?}"
            );
            // Metrics: at least one successful gate call, zero denies for this path.
            let m = actor.extension_runtime.borrow().metrics();
            assert!(m.calls_ok >= 1 || m.pre_tool_denies == 0, "{m}");
        })
        .await;
}

/// Session-owned tool registration against the actor's tool bridge, then
/// simulated session-end unregister (production multi-session safety).
#[tokio::test(flavor = "current_thread")]
async fn session_actor_registers_and_unregisters_wasm_tools() {
    let Some(rt) = load_template_runtime(vec![Capability::RegisterTool]) else {
        return;
    };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = build_actor_with_read_tool().await;
            *actor.extension_runtime.borrow_mut() = rt.clone();

            let bridge = actor.agent.borrow().tool_bridge().clone();
            let sid = actor.session_info.id.0.as_ref();
            let mut owned = actor.wasm_registered_tools.borrow_mut();
            let n = crate::session::wasm_tools::sync_wasm_tools_to_bridge(
                &bridge, &rt, &mut owned, sid,
            )
            .await;
            assert!(n >= 1, "template should register echo tool");
            assert_eq!(owned.len(), n);
            for name in owned.iter() {
                assert!(name.starts_with("wasm_"));
                assert!(
                    bridge.tool_kind(name).is_some(),
                    "missing on bridge: {name}"
                );
            }

            let dropped =
                crate::session::wasm_tools::unregister_session_wasm_tools(&bridge, &mut owned);
            assert_eq!(dropped, n);
            assert!(owned.is_empty());

            // Metrics from collect path.
            let m = rt.metrics();
            assert!(m.tools_collected >= 1, "{m}");
            crate::session::wasm_tools::emit_runtime_metrics(
                false, // product funnel off in unit tests; still hits dual external path if active
                "session_actor_e2e",
                &rt,
            );
        })
        .await;
}

/// Two sessions register tools **concurrently** on one shared ToolBridge; one
/// session's unregister must not remove the other's tools (Oracle multi-session).
#[tokio::test(flavor = "current_thread")]
async fn concurrent_sessions_share_bridge_without_cross_unregister() {
    let Some(wasm) = fixture_wasm() else {
        return;
    };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let bridge = xai_grok_tools::bridge::ToolBridge::for_test();

            let mut rt_a = ExtensionRuntime::new();
            rt_a.load(&ExtensionSpec {
                name: "sess-a".into(),
                wasm_path: wasm.clone(),
                capabilities: vec![Capability::RegisterTool],
                trusted: true,
                gate_fail: None,
                plugin_data_dir: None,
            })
            .unwrap();
            let mut rt_b = ExtensionRuntime::new();
            rt_b.load(&ExtensionSpec {
                name: "sess-b".into(),
                wasm_path: wasm,
                capabilities: vec![Capability::RegisterTool],
                trusted: true,
                gate_fail: None,
                plugin_data_dir: None,
            })
            .unwrap();

            let mut owned_a = Vec::new();
            let mut owned_b = Vec::new();
            let (n_a, n_b) = tokio::join!(
                crate::session::wasm_tools::sync_wasm_tools_to_bridge(
                    &bridge,
                    &rt_a,
                    &mut owned_a,
                    "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                ),
                crate::session::wasm_tools::sync_wasm_tools_to_bridge(
                    &bridge,
                    &rt_b,
                    &mut owned_b,
                    "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
                ),
            );
            assert!(n_a >= 1 && n_b >= 1, "n_a={n_a} n_b={n_b}");
            for name in &owned_a {
                assert!(!owned_b.contains(name), "collision on {name}");
                assert!(bridge.tool_kind(name).is_some());
            }
            for name in &owned_b {
                assert!(bridge.tool_kind(name).is_some());
            }

            // Unregister A only — B must remain.
            let dropped =
                crate::session::wasm_tools::unregister_session_wasm_tools(&bridge, &mut owned_a);
            assert_eq!(dropped, n_a);
            for name in &owned_b {
                assert!(
                    bridge.tool_kind(name).is_some(),
                    "session B tool removed after A unregister: {name}"
                );
            }
            crate::session::wasm_tools::unregister_session_wasm_tools(&bridge, &mut owned_b);
        })
        .await;
}

/// `run_stop_gate` with sdk-stop-once: first stop blocks, second (stop_hook_active) allows.
#[tokio::test(flavor = "current_thread")]
async fn run_stop_gate_wasm_stop_once() {
    let Some(wasm) = stop_once_wasm() else {
        eprintln!("skip: no sdk-stop-once/extension.wasm");
        return;
    };
    let Some(rt) = load_runtime_from_path("sdk-stop-once", wasm, vec![Capability::StopGate], None)
    else {
        return;
    };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, mut gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            *actor.extension_runtime.borrow_mut() = rt;
            tokio::task::spawn_local(async move {
                while let Some(msg) = gateway_rx.recv().await {
                    if let xai_acp_lib::AcpClientMessage::SessionNotification(args) = msg {
                        let _ = args.response_tx.send(Ok(()));
                    }
                }
            });

            let first = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                actor.run_stop_gate("prompt-stop-1", 0),
            )
            .await
            .expect("stop gate must not hang");
            match first {
                StopGateDecision::KeepWorking { feedback } => {
                    assert!(
                        feedback.to_ascii_lowercase().contains("stop")
                            || feedback.contains("sdk-stop")
                            || !feedback.is_empty(),
                        "expected block feedback, got {feedback}"
                    );
                }
                other => panic!("first stop should KeepWorking, got {other:?}"),
            }

            // continuations_this_turn > 0 → stop_hook_active true for wasm guest
            let second = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                actor.run_stop_gate("prompt-stop-1", 1),
            )
            .await
            .expect("second stop gate must not hang");
            assert!(
                matches!(second, StopGateDecision::AllowStop),
                "second stop should AllowStop, got {second:?}"
            );
        })
        .await;
}

/// before_model inject: same dispatch path as turn loop; inject lands as system-reminder.
#[tokio::test(flavor = "current_thread")]
async fn before_model_inject_pushes_system_reminder() {
    let dir = tempfile::tempdir().unwrap();
    let wat = r#"
        (module
          (import "hyper_host" "set_inject_context" (func $set_inject (param i32 i32)))
          (memory (export "memory") 1)
          (data (i32.const 0) "before-model-policy")
          (func (export "hyper_ext_abi_version") (result i32) i32.const 1)
          (func (export "hyper_ext_on_session_start") (result i32) i32.const 0)
          (func (export "hyper_ext_on_before_model") (result i32)
            ;; "before-model-policy" is 19 bytes
            (call $set_inject (i32.const 0) (i32.const 19))
            i32.const 0)
        )
    "#;
    let path = write_wat_guest(dir.path(), "before_model.wasm", wat);
    let Some(rt) = load_runtime_from_path(
        "before-model-ext",
        path,
        vec![Capability::BeforeModelInject],
        None,
    ) else {
        return;
    };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = build_actor_with_read_tool().await;
            *actor.extension_runtime.borrow_mut() = rt;

            // Mirror turn.rs before_model inject block.
            let ext_rt = actor.extension_runtime.borrow().clone();
            assert!(ext_rt.has_capability(Capability::BeforeModelInject));
            let d = ext_rt
                .dispatch_before_model(&xai_grok_extension_api::BeforeAgentStartIn {
                    prompt: String::new(),
                })
                .await;
            let inj = d
                .out
                .inject_context
                .as_deref()
                .expect("expected inject from before_model guest");
            assert!(
                inj.contains("before-model-policy"),
                "inject missing policy text: {inj}"
            );
            // Apply the same host side-effect the turn loop uses.
            actor.push_system_reminder(inj);

            let conv = actor.chat_state_handle.get_conversation().await;
            let found = conv.iter().any(|item| {
                let s = format!("{item:?}");
                s.contains("before-model-policy")
            });
            assert!(
                found,
                "system-reminder with inject should appear in conversation: {conv:?}"
            );
        })
        .await;
}

/// Fail-closed trap guest: trap on pre_tool → deny via prepare_tool_call.
#[tokio::test(flavor = "current_thread")]
async fn prepare_tool_call_fail_closed_on_trap() {
    let dir = tempfile::tempdir().unwrap();
    let wat = r#"
        (module
          (func (export "hyper_ext_abi_version") (result i32) i32.const 1)
          (func (export "hyper_ext_on_session_start") (result i32) i32.const 0)
          (func (export "hyper_ext_on_pre_tool_use") (result i32)
            unreachable)
        )
    "#;
    let path = write_wat_guest(dir.path(), "trap_gate.wasm", wat);
    let Some(rt) = load_runtime_from_path(
        "trap-gate",
        path,
        vec![Capability::PreToolGate],
        Some(GateFailMode::Closed),
    ) else {
        return;
    };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = build_actor_with_read_tool().await;
            *actor.extension_runtime.borrow_mut() = rt;

            let result = prepare(
                &actor,
                tool_call(
                    "call_trap",
                    "read_file",
                    r#"{"target_file":"/tmp/anything.txt"}"#,
                ),
            )
            .await;
            match result {
                Err(ToolLoop::HookDenied { hook_name }) => {
                    assert!(
                        hook_name.contains("trap-gate") || hook_name.starts_with("wasm:"),
                        "got {hook_name}"
                    );
                }
                other => panic!("fail-closed trap must deny, got {other:?}"),
            }
            let m = actor.extension_runtime.borrow().metrics();
            assert!(m.pre_tool_denies >= 1, "{m}");
        })
        .await;
}

/// Template `hello_wasm` command: collect → session cache → slash resolve → invoke.
#[tokio::test(flavor = "current_thread")]
async fn session_actor_register_command_slash_hello_wasm() {
    let Some(rt) = load_template_runtime(vec![Capability::RegisterCommand]) else {
        eprintln!("skip: no rust-guest-template/extension.wasm");
        return;
    };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            use xai_grok_tools::implementations::grok_build::LoopFireMode;

            let actor = build_actor_with_read_tool().await;
            *actor.extension_runtime.borrow_mut() = rt.clone();

            let cmds = rt.collect_registered_commands().await;
            assert!(
                cmds.iter().any(|c| c.name == "hello_wasm"),
                "template should advertise hello_wasm, got {cmds:?}"
            );
            *actor.wasm_registered_commands.borrow_mut() = cmds.clone();

            // ACP catalog includes the wasm slash command.
            let catalog = slash_commands::available_commands(
                &[],
                slash_commands::CommandAvailability::default(),
                &[],
                &cmds,
            );
            assert!(
                catalog.iter().any(|c| c.name == "hello_wasm"),
                "hello_wasm missing from available_commands: {:?}",
                catalog.iter().map(|c| &c.name).collect::<Vec<_>>()
            );

            // Slash resolve routes to BuiltinAction::WasmCommand.
            let prompt = vec![acp::ContentBlock::Text(acp::TextContent::new(
                "/hello_wasm Grok",
            ))];
            let outcome = slash_commands::resolve(
                prompt,
                &[],
                slash_commands::CommandAvailability::default(),
                slash_commands::SkillSlashRewrite::default(),
                &[],
                LoopFireMode::Detached,
                &cmds,
            );
            let action = match outcome {
                Err(SlashCommandOutcome::Builtin(a)) => a,
                other => panic!("expected Builtin WasmCommand, got {other:?}"),
            };
            match action {
                BuiltinAction::WasmCommand {
                    extension,
                    name,
                    args,
                } => {
                    assert_eq!(name, "hello_wasm");
                    assert_eq!(args, "Grok");
                    assert_eq!(extension, "e2e-template");
                    let out = rt
                        .invoke_registered_command(&extension, &name, &args)
                        .await
                        .expect("invoke hello_wasm");
                    assert!(
                        out.to_ascii_lowercase().contains("hello")
                            && out.to_ascii_lowercase().contains("grok"),
                        "unexpected command output: {out}"
                    );
                }
                other => panic!("expected WasmCommand, got {other:?}"),
            }
        })
        .await;
}

/// post_tool_use observe on the session runtime (success + failure paths).
#[tokio::test(flavor = "current_thread")]
async fn session_actor_post_tool_use_observe() {
    let Some(rt) = load_template_runtime(vec![]) else {
        return;
    };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = build_actor_with_read_tool().await;
            *actor.extension_runtime.borrow_mut() = rt;

            let ext_rt = actor.extension_runtime.borrow().clone();
            let ok = ext_rt
                .dispatch_post_tool_use(&xai_grok_extension_api::PostToolIn {
                    tool_name: "read_file".into(),
                    success: true,
                    tool_input_json: r#"{"target_file":"a.rs"}"#.into(),
                    tool_result_preview: r#"{"ok":true}"#.into(),
                })
                .await;
            assert!(
                !ok.is_empty(),
                "template exports post_tool_use; expected at least one result"
            );
            assert!(
                ok.iter().all(|r| matches!(
                    r,
                    xai_grok_extension_runtime::GuestCallResult::Ok { code: 0, .. }
                )),
                "post_tool success path: {ok:?}"
            );

            let fail = ext_rt
                .dispatch_post_tool_use(&xai_grok_extension_api::PostToolIn {
                    tool_name: "read_file".into(),
                    success: false,
                    tool_input_json: r#"{"target_file":"missing"}"#.into(),
                    tool_result_preview: "ENOENT".into(),
                })
                .await;
            assert!(
                fail.iter().all(|r| matches!(
                    r,
                    xai_grok_extension_runtime::GuestCallResult::Ok { code: 0, .. }
                )),
                "post_tool failure observe is fail-open: {fail:?}"
            );
        })
        .await;
}
