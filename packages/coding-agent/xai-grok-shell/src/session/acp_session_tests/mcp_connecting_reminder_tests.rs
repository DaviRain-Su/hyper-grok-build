//! Tests for the "MCP servers currently connecting" reminder rendering (`format_mcp_connecting_reminder`).
//!
//! The delivery-tool wording exists because some headless clients deliver output ONLY through MCP tools.
//! Telling the model to "proceed without" a still-connecting server made it answer in plain text that no user ever saw.
//! The wording is gated on the explicit `startupHints.deliveryTools` opt-in, NOT on `nonInteractive`.
//! Defaults therefore stay unchanged for every client that does not declare delivery tools.
//! SDK/stdio consumers read plain-text responses; subagents report to their parent.

use super::mcp::format_mcp_connecting_reminder;
use super::support::*;
use xai_grok_mcp::servers::McpInitStrategy;

#[test]
fn default_reminder_lists_connecting_servers() {
    let text = format_mcp_connecting_reminder(&["alpha".to_string()], &[]);
    assert!(text.contains("- alpha\n"));
    assert!(!text.contains("alpha__post"));
    assert!(!text.contains("alpha__ask"));
}

#[test]
fn declared_delivery_tools_are_named_in_the_reminder() {
    let text = format_mcp_connecting_reminder(
        &["alpha".to_string(), "beta".to_string()],
        &["alpha__post".to_string(), "alpha__ask".to_string()],
    );
    assert!(text.contains("- alpha\n- beta\n"));
    assert!(text.contains("alpha__post, alpha__ask"));
}

/// A resident `session/load` carrying explicit `startupHints` re-applies the attaching client's policy.
/// The `UpdateAttachPolicy` message is handled by `apply_attach_policy`.
/// The MCP init strategy and delivery tools must track the CURRENT attachment, not the client that originally spawned the actor.
#[tokio::test(flavor = "current_thread")]
async fn apply_attach_policy_tracks_the_current_attachment() {
    // `create_test_actor` spawns local tasks; it must run inside a LocalSet.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
            let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(0, 100_000, 85, gateway_tx, persistence_tx).await;

            // Strategy assertions only hold without the env override (the override deliberately wins over hints; tests must not mutate global env)
            let env_override =
                std::env::var("MCP_INIT_STRATEGY").is_ok_and(|v| !v.trim().is_empty());

            // Interactive attachment with no delivery tools.
            actor.apply_attach_policy(&crate::session::StartupHints::default());
            if !env_override {
                assert_eq!(actor.mcp_strategy.get(), McpInitStrategy::Progressive);
            }
            assert!(actor.delivery_tools.borrow().is_empty());
            assert!(!actor.attach_non_interactive.get());

            // Headless attachment re-applies Blocking and its delivery tools, and the OAuth-interactivity flag follows the attachment
            // A headless re-attach must not run interactive browser OAuth on the MCP re-init
            actor.apply_attach_policy(&crate::session::StartupHints {
                non_interactive: true,
                delivery_tools: vec!["srv__post".to_string()],
                ..Default::default()
            });
            if !env_override {
                assert_eq!(actor.mcp_strategy.get(), McpInitStrategy::Blocking);
            }
            assert_eq!(
                *actor.delivery_tools.borrow(),
                vec!["srv__post".to_string()]
            );
            assert!(actor.attach_non_interactive.get());
        })
        .await;
}

/// A policy-changing re-attach must re-arm the once-per-actor connecting reminder.
/// A latched default reminder must not suppress the delivery wording for a later delivery-tool attachment.
/// An identical re-attach keeps the latch so per-prompt loads don't re-inject each turn.
#[tokio::test(flavor = "current_thread")]
async fn apply_attach_policy_rearms_connecting_reminder_only_on_change() {
    // `create_test_actor` spawns local tasks; it must run inside a LocalSet.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
            let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(0, 100_000, 85, gateway_tx, persistence_tx).await;

            let headless = crate::session::StartupHints {
                non_interactive: true,
                delivery_tools: vec!["srv__post".to_string()],
                ..Default::default()
            };

            // Simulate a reminder already injected for the spawning client.
            actor.mcp_connecting_reminder_injected.set(true);
            actor.apply_attach_policy(&headless);
            assert!(
                !actor.mcp_connecting_reminder_injected.get(),
                "policy change must re-arm the latched reminder"
            );

            // Same policy again: the latch (re-set after an injection) must hold.
            actor.mcp_connecting_reminder_injected.set(true);
            actor.apply_attach_policy(&headless);
            assert!(
                actor.mcp_connecting_reminder_injected.get(),
                "identical re-attach must not re-arm the reminder"
            );
        })
        .await;
}
