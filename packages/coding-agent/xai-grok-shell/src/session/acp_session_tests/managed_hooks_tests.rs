//! A managed-policy hook must not be disabled through `handle_hooks_action`.
//!
//! These tests drive the shipped `handle_hooks_action` with a real registry whose hook carries requirements provenance.
//! The per-hook `Disable` action must be refused and the bulk `ToggleSource` must skip it, both before writing any disable state.
//! The dispatcher-level exemption and the display predicate are covered with a sandboxed `GROK_HOME` in `xai_grok_hooks::dispatcher` tests.

use super::support::*;
use super::*;

use std::sync::Arc;
use tokio::sync::mpsc;

const MANAGED_HOOK: &str = "requirements/system:pre_tool_use[0].hooks[0]";

/// Snapshots and restores the disabled-hooks file wherever the process actually resolves it.
/// A temp `GROK_HOME` alone cannot redirect it: `grok_home()` is `OnceLock`-cached and another test in this binary may have resolved it first.
/// The guard lets the test assert nothing was written, and if the no-disable rule ever regresses it restores the developer's or CI's real file.
struct DisabledHooksGuard {
    path: Option<std::path::PathBuf>,
    before: Option<String>,
}

impl DisabledHooksGuard {
    fn capture() -> Self {
        let path = xai_grok_config::user_grok_home().map(|home| home.join("disabled-hooks"));
        let before = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok());
        Self { path, before }
    }

    fn assert_unchanged(&self) {
        let after = self
            .path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok());
        assert_eq!(
            after, self.before,
            "no disable state may be written for a managed-policy hook"
        );
    }
}

impl Drop for DisabledHooksGuard {
    fn drop(&mut self) {
        let Some(path) = &self.path else { return };
        match &self.before {
            Some(content) => {
                let _ = std::fs::write(path, content);
            }
            None => {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// Builds a registry with one command hook whose provenance is `Requirements`.
fn managed_registry() -> xai_grok_hooks::discovery::HookRegistry {
    xai_grok_hooks::discovery::registry_from_specs_deduped(vec![xai_grok_hooks::config::HookSpec {
        name: MANAGED_HOOK.to_string(),
        event: xai_grok_hooks::event::HookEventName::PreToolUse,
        handler_type: xai_grok_hooks::config::HandlerType::Command,
        configured_matcher: None,
        matcher: None,
        enabled: true,
        command: Some(std::path::PathBuf::from("exit 0")),
        command_raw: Some("exit 0".to_string()),
        url: None,
        url_raw: None,
        timeout_ms: 5000,
        source_dir: std::env::temp_dir(),
        extra_env: std::collections::HashMap::new(),
        layer: xai_grok_hooks::config::HookProvenance::Requirements,
    }])
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(disabled_hooks_file)]
async fn managed_policy_hook_disable_actions_are_refused() {
    let guard = DisabledHooksGuard::capture();
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            *actor.hook_registry.borrow_mut() = Some(Arc::new(managed_registry()));
            let actor = Arc::new(actor);

            let outcome = actor
                .handle_hooks_action(xai_hooks_plugins_types::HooksAction::Disable {
                    hook_name: MANAGED_HOOK.to_string(),
                })
                .await;
            assert_eq!(
                outcome.status,
                xai_hooks_plugins_types::OutcomeStatus::ValidationError,
                "disable of a managed-policy hook must be refused: {}",
                outcome.message
            );
            assert!(
                outcome.message.contains("managed policy"),
                "refusal must say why: {}",
                outcome.message
            );

            let outcome = actor
                .handle_hooks_action(xai_hooks_plugins_types::HooksAction::ToggleSource {
                    hook_names: vec![MANAGED_HOOK.to_string()],
                    disable: true,
                })
                .await;
            assert!(
                outcome.message.contains("enforced by managed policy"),
                "bulk disable must report the managed skip: {}",
                outcome.message
            );
            assert!(
                outcome.message.contains("Disabled 0/1"),
                "no hook may actually be disabled: {}",
                outcome.message
            );
        })
        .await;
    // Both refusals happened before any disable state was written
    guard.assert_unchanged();
}
