//! Focused tests for the reactive managed re-auth routine's deterministic guard
//! rails: owner-scoping and the per-server cooldown gate. The
//! full re-fetch + swap + re-handshake loop is covered at the unit level by the
//! `xai-grok-mcp` and `managed_mcp` tests; here we assert the `SessionActor`
//! wiring around those primitives.

use crate::session::acp_session::support::*;
use crate::session::acp_session::*;
use std::sync::Arc;
use xai_grok_mcp::servers::McpClient;

const MANAGED: &str = "grok_com_testconnector";

async fn make_actor() -> SessionActor {
    let (gw_tx, _gw_rx) = tokio::sync::mpsc::unbounded_channel();
    let (persist_tx, _persist_rx) = tokio::sync::mpsc::unbounded_channel();
    create_test_actor(100, 128_000, 80, gw_tx, persist_tx).await
}

/// Owner-scoping: a session that does NOT own the managed client in
/// `owned_clients` must refuse the in-place swap (a subagent holds the
/// client as a shared Arc and recovers via the leader instead).
#[tokio::test(flavor = "current_thread")]
async fn reactive_managed_reauth_skips_non_owned_client() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_actor().await;
            let err = actor
                .reactive_managed_reauth(MANAGED)
                .await
                .expect_err("non-owned client must not be re-auth'd");
            assert!(
                err.contains("does not own"),
                "expected owner-scope rejection, got: {err}",
            );
        })
        .await;
}

/// With the client owned but no `auth_manager` available, the inner
/// re-fetch fails fast (no network) and the outer routine records exactly
/// one cooldown failure — the next immediate attempt is gated, and the
/// server is not yet parked in the terminal `auth_required` state (one
/// failure is below the attempt cap).
#[tokio::test(flavor = "current_thread")]
async fn reactive_managed_reauth_records_cooldown_on_failure() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_actor().await;
            actor
                .mcp_state
                .lock()
                .await
                .owned_clients
                .insert(MANAGED.to_string(), Arc::new(McpClient::stub(MANAGED)));

            // First attempt: owner + cooldown gates pass, inner fails on the
            // missing auth manager.
            let err = actor
                .reactive_managed_reauth(MANAGED)
                .await
                .expect_err("no auth manager → inner re-fetch must fail");
            assert!(
                err.contains("auth manager"),
                "expected auth-manager failure, got: {err}",
            );

            // The failure armed the cooldown window, so an immediate second
            // attempt is refused by the gate (not retried).
            let err2 = actor
                .reactive_managed_reauth(MANAGED)
                .await
                .expect_err("cooldown must gate the immediate retry");
            assert!(
                err2.contains("cooldown"),
                "expected cooldown rejection, got: {err2}",
            );

            // A single failure is below the terminal attempt cap, so the
            // server is not yet surfaced as needs-auth.
            assert!(
                !actor.mcp_state.lock().await.auth_required.contains(MANAGED),
                "one failure must not park the server in auth_required",
            );
            assert!(
                !actor
                    .managed_mcp_handle
                    .lock()
                    .await
                    .reauth_is_terminal(MANAGED),
                "one failure must not be terminal",
            );
        })
        .await;
}
