//! Cross-suite e2e flow helpers over [`PtyHarness`] / [`ContentController`].
//!
//! Driving/seeding helpers shared by the pager's `pty_e2e` and `leader_pty_e2e` test targets (both depend on this crate).
//! Suite-local constants (sizes, sentinels, timeouts) stay in each suite's `common.rs`.

use std::time::{Duration, Instant};

use crate::{ContentController, PtyHarness};

/// Pump PTY output until every label is absent from the visible screen.
pub fn wait_for_labels_absent(h: &mut PtyHarness, labels: &[&str], timeout: Duration) {
    let _ = h.wait_until("screen labels to disappear", timeout, |h| {
        labels.iter().all(|label| !h.contains_text(label))
    });
}

/// Submit `prompt` from `h`, then keep re-pressing Enter until the turn actually starts streaming (`sentinel` appears) or `timeout` elapses.
///
/// In a heavy multi-client leader cluster, the driver's submit Enter can be dropped when it races the other client's attach/replay on the leader.
/// The typed prompt is left sitting unsubmitted in the composer, the turn never starts, and a plain `wait_for_text` then times out.
/// That was the `leader_two_clients_shared_session` flake: client A idle with `again` still in the composer at 75s.
/// Re-pressing Enter is safe: submitting takes the composer draft synchronously (`std::mem::take` in `dispatch`).
/// Once a turn has really been sent the composer is empty and an extra Enter is a no-op.
/// It can only submit a still-stuck prompt, never double-submit a sent one (which would break exactly-once scrollback asserts).
pub fn submit_turn(h: &mut PtyHarness, prompt: &str, sentinel: &str, timeout: Duration) {
    h.inject_keys(format!("{prompt}\r").as_bytes())
        .expect("inject prompt submit");
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        // Each attempt gets its own budget, generous enough that a genuinely in-progress submit resolves before we press Enter again
        // The extra Enter then only ever fires on an empty composer, where it is a no-op
        if h.wait_for_text(sentinel, Duration::from_secs(10).min(remaining))
            .is_ok()
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {timeout:?} waiting for {sentinel:?}\nscreen:\n{}",
            h.screen_contents()
        );
        let _ = h.inject_keys(b"\r");
    }
}

/// Count only inference requests (chat completions / responses / messages), ignoring incidental GETs like /v1/models and /v1/settings.
/// A replay invariant then means "no turn was re-driven" rather than "no HTTP at all".
pub fn inference_request_count(content: &ContentController) -> usize {
    content
        .requests()
        .iter()
        .filter(|e| {
            e.path.contains("/chat/completions")
                || e.path.contains("/responses")
                || e.path.contains("/messages")
        })
        .count()
}

/// Seed a fake xAI OAuth entry into the isolated home's `auth.json` so the shell has session auth.
/// The harness's `XAI_API_KEY` is ApiKey/BYOK mode and never enters the auth manager.
/// The scope key must be `<issuer>::<client_id>`, `auth_mode` must be `oidc`, and `expires_at` must be far-future so no network refresh happens.
/// `coding_data_retention_opt_out` must be `false` so collection/upload-path e2es (e.g. storage park-on-401) still enqueue traces.
/// When that field is missing it deserializes as opted-out via `default_coding_data_retention_opt_out()`.
/// The mock server accepts any bearer.
/// Pair with [`oauth_credential_ops`].
pub fn seed_fake_oauth(content: &ContentController, user: &str) {
    seed_fake_oauth_with_opt_out(content, user, false);
}

/// Like [`seed_fake_oauth`], but with `coding_data_retention_opt_out: true`.
/// That is the auth-side precondition for the coding-data privacy upsell banner.
pub fn seed_fake_oauth_coding_data_opted_out(content: &ContentController, user: &str) {
    seed_fake_oauth_with_opt_out(content, user, true);
}

/// Like [`seed_fake_oauth_coding_data_opted_out`], but on a Zero Data Retention team.
/// `team_blocked_reasons` carries `BLOCKED_REASON_NO_LOGS`, the shell's `GrokAuth::is_zdr_team` trigger.
/// This locks the settings modal's `coding_data_sharing` row to `ZDR` and suppresses the privacy banner.
pub fn seed_fake_oauth_zdr_team(content: &ContentController, user: &str) {
    seed_fake_oauth_raw(
        content,
        user,
        true,
        ",\n    \"team_name\": \"PTY ZDR Team\",\n    \"team_role\": \"MEMBER\",\n    \
         \"team_blocked_reasons\": [\"BLOCKED_REASON_NO_LOGS\"]",
    );
}

/// Like [`seed_fake_oauth_coding_data_opted_out`], but as a non-admin member of a (non-ZDR) team.
/// This locks the settings modal's `coding_data_sharing` row to `Opt out · Admin Managed` and suppresses the privacy banner.
pub fn seed_fake_oauth_team_member(content: &ContentController, user: &str) {
    seed_fake_oauth_raw(
        content,
        user,
        true,
        ",\n    \"team_name\": \"PTY Team\",\n    \"team_role\": \"MEMBER\"",
    );
}

fn seed_fake_oauth_with_opt_out(content: &ContentController, user: &str, opted_out: bool) {
    seed_fake_oauth_raw(content, user, opted_out, "");
}

/// Shared auth.json template writer.
/// `team_fields` is a raw JSON fragment spliced after `coding_data_retention_opt_out` (empty means no team).
/// Field names must match the shell's `GrokAuth` serde names in `xai-grok-shell/src/auth/model.rs`.
fn seed_fake_oauth_raw(
    content: &ContentController,
    user: &str,
    opted_out: bool,
    team_fields: &str,
) {
    let grok_home = content.home().join(".grok");
    std::fs::create_dir_all(&grok_home).expect("create temp .grok");
    std::fs::write(
        grok_home.join("auth.json"),
        format!(
            r#"{{
  "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828": {{
    "key": "pty-test-oauth-token",
    "auth_mode": "oidc",
    "create_time": "2026-01-01T00:00:00Z",
    "user_id": "{user}",
    "email": "{user}@test.invalid",
    "expires_at": "2030-01-01T00:00:00Z",
    "refresh_token": "pty-test-refresh-token",
    "oidc_issuer": "https://auth.x.ai",
    "oidc_client_id": "b1a00492-073a-47ea-816f-4c329264a828",
    "coding_data_retention_opt_out": {opted_out}{team_fields}
  }}
}}"#
        ),
    )
    .expect("seed fake oauth auth.json");
}

/// Remove only the sandbox's fake API-key credential.
/// The `auth.json` entry written by [`seed_fake_oauth`] then determines the advertised auth method.
pub fn oauth_credential_ops() -> [crate::EnvOp<'static>; 1] {
    [crate::EnvOp::remove("XAI_API_KEY")]
}

/// Drive `/new` until `model` shows on screen.
/// Campaigns apply to **new sessions only** and the pager's settings prefetch is deliberately 2s-capped.
/// On a loaded runner the first session can legitimately open without the campaign applied.
/// Each `/new` after the settings fetch lands re-resolves with the campaign.
pub fn wait_for_model_via_new_sessions(h: &mut PtyHarness, model: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if h.contains_text(model) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        let _ = h.inject_keys(b"/new\r");
        h.update(Duration::from_millis(3000));
    }
}
