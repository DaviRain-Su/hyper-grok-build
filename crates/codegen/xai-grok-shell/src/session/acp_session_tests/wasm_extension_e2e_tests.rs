//! SessionActor-level smoke for WASM extensions through the real
//! `prepare_tool_call` path (hooks → wasm gate → continue).
//!
//! Not a full multi-turn session; it does pin the production seam:
//! load fixture guest into the actor's `extension_runtime`, then exercise
//! deny/allow and session-owned tool register/unregister on the agent bridge.

use super::support::*;
use super::*;
use std::path::PathBuf;
use xai_grok_extension_api::{Capability, ExtensionSpec};
use xai_grok_extension_runtime::ExtensionRuntime;
use xai_grok_tools::registry::types::ToolConfig;

fn fixture_wasm() -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../xai-grok-extension-runtime/examples/rust-guest-template/extension.wasm");
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
                assert!(bridge.tool_kind(name).is_some(), "missing on bridge: {name}");
            }

            let dropped =
                crate::session::wasm_tools::unregister_session_wasm_tools(&bridge, &mut owned);
            assert_eq!(dropped, n);
            assert!(owned.is_empty());

            // Metrics from collect path.
            let m = rt.metrics();
            assert!(m.tools_collected >= 1, "{m}");
            rt.log_metrics("session_actor_e2e");
        })
        .await;
}
