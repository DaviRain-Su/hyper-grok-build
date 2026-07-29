//! Tests for session status, sharing, privacy, and coding-data-sharing dispatchers.

use super::*;

/// Regression (leader-mode turn-end race): when this client is briefly Idle
/// (`is_turn_running() == false`, `current_prompt_id` cleared) but the server
/// still has queued prompts — visible as a non-empty `shared_queue` mirror —
/// a newly-sent prompt must route to the SERVER (immediate-send), NOT be
/// locally drained as a phantom running turn. The failure mode: a
/// `send_route_plain immediate=false is_turn_running=false shared_queue_len=5`
/// path taking `local_drain`, leaving the prompt shown running on the sender
/// while it was actually queued behind the existing entries on the leader and
/// every other client.
#[test]
fn send_while_idle_with_nonempty_shared_queue_routes_to_server() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    // Two prompts already queued on the server (as a broadcast would leave
    // things): populate the authoritative map AND mirror it into the agent.
    app.push_optimistic_prompt_echo("test-session", "q1", "a", "prompt");
    app.push_optimistic_prompt_echo("test-session", "q2", "b", "prompt");
    {
        let snapshot = app.shared_prompt_queue("test-session").cloned().unwrap();
        let agent = app.agents.get_mut(&id).unwrap();
        // Turn-end window: locally Idle with no current prompt, but the
        // server's queue (mirrored from the last broadcast) still has work.
        agent.session.state = AgentState::Idle;
        agent.session.current_prompt_id = None;
        agent.shared_queue = snapshot;
        assert!(agent.session.pending_prompts.is_empty());
    }

    let effects = dispatch(Action::SendPrompt("c".into()), &mut app);

    // Routed to the server (immediate-send), keyed by a fresh prompt_id.
    let pid = effects
        .iter()
        .find_map(|e| match e {
            Effect::SendPrompt {
                text, prompt_id, ..
            } if text == "c" => Some(prompt_id.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected immediate SendPrompt for 'c', got {effects:?}"));
    // Did NOT start a local turn or adopt "c" as the running prompt.
    assert!(
        !app.agents[&id].session.state.is_turn_running(),
        "must not promote 'c' to a local running turn"
    );
    assert!(
        app.agents[&id].session.current_prompt_id.is_none(),
        "must not set current_prompt_id locally for a server-queued prompt"
    );
    // Echoed into the shared queue BEHIND the existing entries (position 3).
    let q = app
        .shared_prompt_queue("test-session")
        .expect("optimistic echo present");
    assert_eq!(q.len(), 3, "c queued behind q1, q2");
    assert_eq!(q.last().map(|e| e.id.as_str()), Some(pid.as_str()));
    assert_eq!(q.last().map(|e| e.text.as_str()), Some("c"));
}

// ── coding_data_sharing dispatch tests ───
//
// The dispatcher uses **optimistic + rollback**, matching the
// `set_yolo_mode` pattern minus its toasts — the surfaces that change this
// setting show the result themselves. These tests pin the contract:
//   - Guards (ZDR, non-admin team) toast and short-circuit; they are the
//     only paths that still speak up, because nothing else on screen would.
//   - Idempotent dispatch emits no Effect and says nothing.
//   - Optimistic mutation flips `app.coding_data_retention_opt_out`
//     BEFORE the Effect is emitted.
//   - `Effect::SetCodingDataSharing` carries
//     `rollback_to_opted_in = previous_value`.
//   - `TaskResult::CodingDataSharingFailed` reverts the optimistic
//     mutation; `TaskResult::CodingDataSharingUpdated` re-anchors
//     to the server-confirmed value.

/// Idempotent re-dispatch skips the ACP round-trip.
#[test]
fn set_coding_data_sharing_idempotent_is_silent_and_effect_free() {
    for opted_in in [true, false] {
        let mut app = test_app_with_agent();
        app.coding_data_retention_opt_out = !opted_in; // already at the target
        let effects = dispatch(Action::SetCodingDataSharing { opted_in }, &mut app);
        assert!(
            effects.is_empty(),
            "idempotent re-dispatch must NOT emit Effect (opted_in={opted_in})"
        );
        assert!(
            app.agents[&AgentId(0)].toast.is_none(),
            "idempotent re-dispatch must not toast (opted_in={opted_in})"
        );
        assert_eq!(
            app.coding_data_retention_opt_out, !opted_in,
            "idempotent path must not mutate state (opted_in={opted_in})",
        );
    }
}

/// ZDR teams are blocked from toggling. The blocked path
/// toasts (not scrollback) and short-circuits with no Effect.
#[test]
fn set_coding_data_sharing_blocked_by_zdr() {
    let mut app = test_app_with_agent();
    app.is_zdr = true;
    app.coding_data_retention_opt_out = false;
    let effects = dispatch(Action::SetCodingDataSharing { opted_in: false }, &mut app);
    assert!(effects.is_empty(), "ZDR block must NOT emit Effect");
    let toast = read_toast(&app);
    assert!(
        toast.contains("Zero Data Retention"),
        "ZDR toast must surface the policy: {toast}",
    );
    assert!(
        toast.contains('\u{2717}'),
        "blocked toast uses ✗ glyph: {toast}"
    );
    // State unchanged — the user was blocked, the optimistic
    // mutation never happened.
    assert!(
        !app.coding_data_retention_opt_out,
        "ZDR block must not mutate state",
    );
}

/// ZDR block fires even when the toggle would be a no-op
/// (defense-in-depth: don't quietly accept a same-value toggle
/// from a user the policy says shouldn't be touching this).
#[test]
fn set_coding_data_sharing_blocked_by_zdr_even_if_idempotent() {
    let mut app = test_app_with_agent();
    app.is_zdr = true;
    app.coding_data_retention_opt_out = false;
    let effects = dispatch(Action::SetCodingDataSharing { opted_in: true }, &mut app);
    assert!(effects.is_empty());
    assert!(read_toast(&app).contains("Zero Data Retention"));
}

/// Non-admin team members are blocked from toggling (matches
/// desktop). The blocked path toasts and short-circuits.
#[test]
fn set_coding_data_sharing_blocked_non_admin() {
    let mut app = test_app_with_agent();
    app.team_name = Some("Acme".into());
    app.team_role = Some("Member".into());
    app.coding_data_retention_opt_out = false;
    let effects = dispatch(Action::SetCodingDataSharing { opted_in: false }, &mut app);
    assert!(effects.is_empty());
    let toast = read_toast(&app);
    assert!(
        toast.contains("team admin"),
        "non-admin toast must mention team admin: {toast}",
    );
}

/// Admin team members CAN toggle. The admin-allowed path produces
/// an Effect carrying the rollback value.
#[test]
fn set_coding_data_sharing_allowed_for_admin() {
    let mut app = test_app_with_agent();
    app.team_name = Some("Acme".into());
    app.team_role = Some("Admin".into());
    app.coding_data_retention_opt_out = false; // currently opted-in
    let effects = dispatch(Action::SetCodingDataSharing { opted_in: false }, &mut app);
    assert_eq!(effects.len(), 1);
    match &effects[0] {
        Effect::SetCodingDataSharing {
            opted_in,
            rollback_to_opted_in,
            ..
        } => {
            assert!(!*opted_in, "Effect must carry opted_in=false");
            assert!(
                *rollback_to_opted_in,
                "rollback_to_opted_in must capture pre-toggle opt-in=true",
            );
        }
        other => panic!("expected SetCodingDataSharing Effect, got {other:?}"),
    }
    // Optimistic mutation already applied.
    assert!(
        app.coding_data_retention_opt_out,
        "admin-allowed dispatch must optimistically flip state",
    );
}

/// Non-idempotent dispatch emits one Effect AND mutates state
/// optimistically.
#[test]
fn set_coding_data_sharing_produces_effect_and_optimistic_mutation() {
    let mut app = test_app_with_agent();
    app.coding_data_retention_opt_out = false; // currently opted-in
    let effects = dispatch(Action::SetCodingDataSharing { opted_in: false }, &mut app);
    assert_eq!(effects.len(), 1, "non-idempotent dispatch emits one Effect");
    match &effects[0] {
        Effect::SetCodingDataSharing {
            agent_id,
            opted_in,
            rollback_to_opted_in,
            seq,
        } => {
            assert_eq!(*agent_id, AgentId(0));
            assert!(!*opted_in);
            assert!(
                *rollback_to_opted_in,
                "rollback_to_opted_in must be pre-toggle value (true == opted-in)",
            );
            assert_eq!(
                *seq, app.coding_data_write_seq,
                "the effect must carry the generation it was dispatched under",
            );
        }
        other => panic!("expected SetCodingDataSharing Effect, got {other:?}"),
    }
    // Optimistic mutation applied.
    assert!(
        app.coding_data_retention_opt_out,
        "dispatch must optimistically mutate state",
    );
    assert!(
        app.agents[&AgentId(0)].toast.is_none(),
        "changing this setting must not toast — the settings row is the feedback",
    );
}

/// `TaskResult::CodingDataSharingUpdated` re-anchors state to the
/// server-confirmed value (defense-in-depth).
#[test]
fn coding_data_sharing_updated_re_anchors_state() {
    let mut app = test_app_with_agent();
    // Simulate post-optimistic state: opted-out.
    app.coding_data_retention_opt_out = true;
    let id = AgentId(0);
    // Server confirms opt-out (same as optimistic).
    let seq = app.coding_data_write_seq;
    let effects = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingUpdated {
            agent_id: id,
            opted_in: false,
            seq,
        }),
        &mut app,
    );
    assert!(effects.is_empty(), "TaskResult arm must NOT emit Effect");
    // State re-anchored (was already true, stays true).
    assert!(app.coding_data_retention_opt_out);
    assert!(
        app.agents[&AgentId(0)].toast.is_none(),
        "server confirmation must not toast",
    );
}

/// `TaskResult::CodingDataSharingUpdated` corrects the in-memory
/// state if the server reshapes the boolean (e.g. policy
/// override). Pins the defense-in-depth re-anchor contract.
#[test]
fn coding_data_sharing_updated_corrects_state_if_server_disagrees() {
    let mut app = test_app_with_agent();
    // Optimistic mutation said "opt-out" — but the server
    // overrides to "opt-in" (e.g. policy that prevents opt-out).
    app.coding_data_retention_opt_out = true;
    let id = AgentId(0);
    let seq = app.coding_data_write_seq;
    let effects = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingUpdated {
            agent_id: id,
            opted_in: true, // server says opted-in
            seq,
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    // State corrected to match server.
    assert!(
        !app.coding_data_retention_opt_out,
        "server-confirmed opt-in must overwrite optimistic opt-out",
    );
}

/// `TaskResult::CodingDataSharingFailed` REVERTS the optimistic
/// mutation and surfaces a failure toast. Pins the rollback
/// contract.
///
/// Failure toast uses the standardised "coding data sharing"
/// wording.
#[test]
fn coding_data_sharing_failed_rolls_back_and_toasts_error() {
    let mut app = test_app_with_agent();
    // Simulate post-optimistic state: user picked opt-out, state
    // was flipped, then the ACP call failed. The pre-toggle value
    // was opt-in (true), so `rollback_to_opted_in = true`.
    app.coding_data_retention_opt_out = true;
    let id = AgentId(0);
    let seq = app.coding_data_write_seq;
    let effects = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: id,
            error: "server error".into(),
            rollback_to_opted_in: true,
            seq,
        }),
        &mut app,
    );
    assert!(effects.is_empty(), "rollback path must NOT emit Effect");
    // State reverted to pre-toggle (opted-in).
    assert!(
        !app.coding_data_retention_opt_out,
        "rollback must revert optimistic mutation",
    );
    // Failure toast surfaces the error using full label.
    let toast = read_toast(&app);
    assert!(
        toast.contains("coding data sharing"),
        "PR 9 R1: failure toast wording standardised to include 'coding data sharing' \
             (G2 Issue 2): {toast}",
    );
    assert!(toast.contains("server error"), "error in toast: {toast}");
    assert!(toast.contains('\u{2717}'), "failure toast uses ✗: {toast}");
}

/// `TaskResult::CodingDataSharingFailed` reverts in the OTHER
/// direction too (the pre-toggle state could have been either).
#[test]
fn coding_data_sharing_failed_rolls_back_to_opt_out() {
    let mut app = test_app_with_agent();
    // Post-optimistic: opted-in (user picked opt-in, server
    // failed, pre-toggle was opt-out).
    app.coding_data_retention_opt_out = false;
    let id = AgentId(0);
    let seq = app.coding_data_write_seq;
    let effects = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: id,
            error: "network timeout".into(),
            rollback_to_opted_in: false,
            seq,
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    // Reverted to pre-toggle opt-out.
    assert!(
        app.coding_data_retention_opt_out,
        "rollback to opt-out must set state=true",
    );
}

/// Optimistic mutation refreshes any open settings modal.
/// Without this refresh, the modal indicator would stay at the
/// pre-toggle value until manual re-render.
#[test]
fn set_coding_data_sharing_refreshes_open_modal_snapshot() {
    let mut app = test_app_with_agent();
    app.coding_data_retention_opt_out = false;
    // Open a settings modal (capture initial snapshot).
    let _ = dispatch(Action::OpenSettings, &mut app);
    // Verify snapshot reads opted-in.
    let agent_id = AgentId(0);
    {
        let state = match &app.agents[&agent_id].active_modal {
            Some(crate::views::modal::ActiveModal::Settings { state }) => state,
            _ => panic!("expected Settings modal open after OpenSettings dispatch"),
        };
        assert!(
            !state.pager_snapshot.coding_data_sharing_opt_out,
            "initial snapshot must read opt_out=false (opted-in)",
        );
    }
    // Dispatch the toggle.
    let _ = dispatch(Action::SetCodingDataSharing { opted_in: false }, &mut app);
    // Snapshot now reflects the optimistic mutation.
    let state = match &app.agents[&agent_id].active_modal {
        Some(crate::views::modal::ActiveModal::Settings { state }) => state,
        _ => panic!("Settings modal must still be open after SetCodingDataSharing dispatch"),
    };
    assert!(
        state.pager_snapshot.coding_data_sharing_opt_out,
        "snapshot must refresh to reflect opt_out=true (opted-out) after dispatch",
    );
}

/// Rollback also refreshes the modal — the user sees the
/// reverted value, not the stale optimistic one.
#[test]
fn coding_data_sharing_failed_refreshes_open_modal_snapshot() {
    let mut app = test_app_with_agent();
    app.coding_data_retention_opt_out = false;
    let _ = dispatch(Action::OpenSettings, &mut app);
    // Optimistic flip.
    let _ = dispatch(Action::SetCodingDataSharing { opted_in: false }, &mut app);
    // ACP failure.
    let seq = app.coding_data_write_seq;
    let _ = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: AgentId(0),
            error: "x".into(),
            rollback_to_opted_in: true,
            seq,
        }),
        &mut app,
    );
    let state = match &app.agents[&AgentId(0)].active_modal {
        Some(crate::views::modal::ActiveModal::Settings { state }) => state,
        _ => panic!("Settings modal must still be open after rollback TaskResult"),
    };
    assert!(
        !state.pager_snapshot.coding_data_sharing_opt_out,
        "rollback must refresh snapshot back to opt_out=false (opted-in)",
    );
}

#[test]
fn set_coding_data_sharing_is_silent_in_both_directions() {
    for opted_in in [true, false] {
        let mut app = test_app_with_agent();
        app.coding_data_retention_opt_out = opted_in; // a real change either way
        let _ = dispatch(Action::SetCodingDataSharing { opted_in }, &mut app);
        assert!(
            app.agents[&AgentId(0)].toast.is_none(),
            "opted_in={opted_in} must not toast, got {:?}",
            app.agents[&AgentId(0)].toast,
        );
    }
}

/// The failure toast
/// substitutes a generic placeholder when the error string is
/// too long OR contains control characters / newlines. Pins the
/// scrub contract.
#[test]
fn coding_data_sharing_failed_scrubs_long_error_messages() {
    let mut app = test_app_with_agent();
    app.coding_data_retention_opt_out = true;
    let id = AgentId(0);
    // ~500-char error simulating a stack trace / HTML 502 page.
    let huge_error = "a".repeat(500);
    let seq = app.coding_data_write_seq;
    let _ = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: id,
            error: huge_error.clone(),
            rollback_to_opted_in: false,
            seq,
        }),
        &mut app,
    );
    let toast = read_toast(&app);
    assert!(
        !toast.contains(&huge_error),
        "long error MUST be scrubbed from the toast: {} chars",
        toast.len(),
    );
    assert!(
        toast.contains("see logs"),
        "scrubbed toast must point at the log for full details: {toast}",
    );
}

/// Control characters (CR/LF/NUL)
/// in the error trigger the scrub path even on short strings —
/// preserves the toast's single-line layout.
#[test]
fn coding_data_sharing_failed_scrubs_control_chars_in_error() {
    let mut app = test_app_with_agent();
    app.coding_data_retention_opt_out = true;
    let id = AgentId(0);
    // Short message with embedded newlines.
    let multiline = "line1\nline2\nline3".to_string();
    let seq = app.coding_data_write_seq;
    let _ = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: id,
            error: multiline.clone(),
            rollback_to_opted_in: false,
            seq,
        }),
        &mut app,
    );
    let toast = read_toast(&app);
    assert!(
        !toast.contains('\n'),
        "newlines MUST be scrubbed from the toast (would break single-line layout): \
             {toast:?}",
    );
    assert!(
        toast.contains("see logs"),
        "control-char-scrubbed toast points at logs: {toast}",
    );
}

/// The scrub path preserves short,
/// sanitised error messages verbatim — the typical happy-path
/// shell-side error string stays unscrubbed.
#[test]
fn coding_data_sharing_failed_preserves_short_clean_error_message() {
    let mut app = test_app_with_agent();
    app.coding_data_retention_opt_out = true;
    let id = AgentId(0);
    let short_clean = "network timeout".to_string();
    let seq = app.coding_data_write_seq;
    let _ = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: id,
            error: short_clean.clone(),
            rollback_to_opted_in: false,
            seq,
        }),
        &mut app,
    );
    let toast = read_toast(&app);
    assert!(
        toast.contains(&short_clean),
        "short clean error must appear verbatim in the toast: {toast}",
    );
    assert!(
        !toast.contains("see logs"),
        "short clean error must NOT trigger the scrub fallback: {toast}",
    );
}

/// Direct unit test of the `scrub_error_for_toast` helper —
/// pins the threshold and the fallback string against drift.
#[test]
fn scrub_error_for_toast_unit() {
    // Empty + short messages pass through.
    assert_eq!(scrub_error_for_toast(""), "");
    assert_eq!(scrub_error_for_toast("ok"), "ok");
    assert_eq!(scrub_error_for_toast("network timeout"), "network timeout");
    // At-threshold (120 chars) still passes through.
    let len_120 = "x".repeat(120);
    assert_eq!(scrub_error_for_toast(&len_120), len_120);
    // Over-threshold (121 chars) triggers scrub.
    let len_121 = "x".repeat(121);
    assert_eq!(
        scrub_error_for_toast(&len_121),
        "server error (see logs for details)"
    );
    // Control chars trigger scrub even at short lengths.
    assert_eq!(
        scrub_error_for_toast("hi\nthere"),
        "server error (see logs for details)"
    );
    assert_eq!(
        scrub_error_for_toast("hi\rthere"),
        "server error (see logs for details)"
    );
    // Format-category (Cf) chars also trigger scrub — bidi
    // overrides, zero-width joiner / space, BOM. Prevents
    // Trojan-Source-style visual spoofing
    // where a toast READS as one thing but bytes encode
    // another via embedded RIGHT-TO-LEFT-OVERRIDE.
    assert_eq!(
        scrub_error_for_toast("opt\u{202E}-out"),
        "server error (see logs for details)",
        "RIGHT-TO-LEFT OVERRIDE (U+202E) must be scrubbed",
    );
    assert_eq!(
        scrub_error_for_toast("opt\u{200B}out"),
        "server error (see logs for details)",
        "ZERO WIDTH SPACE (U+200B) must be scrubbed",
    );
    assert_eq!(
        scrub_error_for_toast("\u{FEFF}leading BOM"),
        "server error (see logs for details)",
        "BOM (U+FEFF) must be scrubbed",
    );
    assert_eq!(
        scrub_error_for_toast("zwj\u{200D}joiner"),
        "server error (see logs for details)",
        "ZERO WIDTH JOINER (U+200D) must be scrubbed",
    );
}

/// Synthetic AgentId(0) when no agents (welcome banner Accept path).
#[test]
fn set_coding_data_sharing_no_agents_still_emits_effect() {
    let mut app = test_app_with_agent();
    app.agents.clear();
    app.active_view = ActiveView::Welcome;
    app.coding_data_retention_opt_out = true;
    let effects = dispatch(Action::SetCodingDataSharing { opted_in: true }, &mut app);
    assert_eq!(effects.len(), 1, "no-agent path must still emit Effect");
    assert!(
        !app.coding_data_retention_opt_out,
        "optimistic opt-in must apply without agents",
    );
}

fn privacy_banner_ready_app() -> AppView {
    let mut app = test_app_with_agent();
    app.active_view = ActiveView::Welcome;
    app.auth_state = AuthState::Done;
    app.trust_state = TrustState::Done;
    app.privacy_notice_rollout = true;
    app.privacy_banner_acked = None;
    app.privacy_banner_reshow_days = None;
    app.privacy_banner_opt_in_inflight = false;
    app.is_zdr = false;
    app.team_name = None;
    app.coding_data_retention_opt_out = true;
    app
}

#[test]
fn privacy_banner_should_show_respects_gates() {
    let mut app = privacy_banner_ready_app();
    assert!(app.privacy_banner_should_show());

    app.coding_data_retention_opt_out = false;
    assert!(!app.privacy_banner_should_show(), "already opted in");
    app.coding_data_retention_opt_out = true;

    app.is_zdr = true;
    assert!(!app.privacy_banner_should_show(), "enterprise ZDR");
    app.is_zdr = false;

    app.privacy_banner_acked = Some("2099-01-01T00:00:00Z".into());
    assert!(
        !app.privacy_banner_should_show(),
        "recently acked, no reshow"
    );

    app.privacy_banner_reshow_days = Some(30);
    app.privacy_banner_acked = Some("2020-01-01T00:00:00Z".into());
    assert!(
        app.privacy_banner_should_show(),
        "acked long ago + reshow_days"
    );

    app.privacy_notice_rollout = false;
    assert!(!app.privacy_banner_should_show(), "rollout off");
}

/// `[Opt in]` success: ACP confirmation acks the banner.
#[test]
fn privacy_banner_opt_in_success_acks() {
    let mut app = privacy_banner_ready_app();
    let effects = dispatch(Action::PrivacyBannerOptIn, &mut app);
    assert_eq!(effects.len(), 1);
    assert!(matches!(
        &effects[0],
        Effect::SetCodingDataSharing { opted_in: true, .. }
    ));
    assert!(app.privacy_banner_opt_in_inflight);
    assert!(!app.coding_data_retention_opt_out);
    assert!(app.privacy_banner_acked.is_none());

    let seq = app.coding_data_write_seq;
    let ack_effects = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingUpdated {
            agent_id: AgentId(0),
            opted_in: true,
            seq,
        }),
        &mut app,
    );
    assert!(!app.privacy_banner_opt_in_inflight);
    assert!(app.privacy_banner_acked.is_some());
    assert!(
        ack_effects
            .iter()
            .any(|e| matches!(e, Effect::PersistPrivacyBannerAcked { .. })),
        "success must persist ack: {ack_effects:?}"
    );
}

/// `[Opt in]` failure: no ack; welcome toast carries the error.
#[test]
fn privacy_banner_opt_in_failure_no_ack_sets_welcome_toast() {
    let mut app = privacy_banner_ready_app();
    let effects = dispatch(Action::PrivacyBannerOptIn, &mut app);
    assert_eq!(effects.len(), 1);
    assert!(app.privacy_banner_opt_in_inflight);

    let seq = app.coding_data_write_seq;
    let fail_effects = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: AgentId(0),
            error: "server error".into(),
            rollback_to_opted_in: false,
            seq,
        }),
        &mut app,
    );
    assert!(fail_effects.is_empty());
    assert!(!app.privacy_banner_opt_in_inflight);
    assert!(app.privacy_banner_acked.is_none());
    assert!(
        app.coding_data_retention_opt_out,
        "rollback restores opt-out"
    );
    let toast = app
        .welcome_toast
        .as_ref()
        .map(|(m, _)| m.as_str())
        .unwrap_or("");
    assert!(
        toast.contains("coding data sharing"),
        "welcome toast on [Opt in] failure: {toast}"
    );
    assert!(toast.contains("server error"), "error in toast: {toast}");
}

/// `[Opt out]` while an `[Opt in]` ACP call is inflight must be a no-op:
/// an eager ack would survive the opt-in-failure rollback and hide the
/// banner forever.
#[test]
fn privacy_banner_opt_out_noop_while_opt_in_inflight() {
    let mut app = privacy_banner_ready_app();
    let _ = dispatch(Action::PrivacyBannerOptIn, &mut app);
    assert!(app.privacy_banner_opt_in_inflight);

    let effects = dispatch(Action::PrivacyBannerOptOut, &mut app);
    assert!(
        effects.is_empty(),
        "[Opt out] during an inflight [Opt in] must be a no-op: {effects:?}"
    );
    assert!(app.privacy_banner_acked.is_none(), "no ack while inflight");

    let seq = app.coding_data_write_seq;
    let _ = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: AgentId(0),
            error: "server error".into(),
            rollback_to_opted_in: false,
            seq,
        }),
        &mut app,
    );
    assert!(
        app.privacy_banner_should_show(),
        "a failed [Opt in] must keep the banner even after a raced [Opt out]"
    );
}

/// The ack must not hinge on the round trip, unlike `[Opt in]`'s.
#[test]
fn privacy_banner_opt_out_acks_now_and_records_decline() {
    use crate::views::modal::ActiveModal;
    let mut app = privacy_banner_ready_app();

    let effects = dispatch(Action::PrivacyBannerOptOut, &mut app);

    assert!(
        app.privacy_banner_acked.is_some(),
        "the ack lands on click, not on an ACP reply"
    );
    assert!(
        !app.privacy_banner_should_show(),
        "the banner is gone the moment it is dismissed"
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::PersistPrivacyBannerAcked { .. })),
        "ack must persist: {effects:?}"
    );
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::SetCodingDataSharing {
                opted_in: false,
                rollback_to_opted_in: false,
                ..
            }
        )),
        "the decline rides the ordinary write, so its response re-anchors \
         the mirror like every other one: {effects:?}"
    );
    assert!(
        !app.privacy_banner_opt_in_inflight,
        "a best-effort write must not arm the opt-in inflight guard, which \
         would block [Opt in] and confuse both ACP result handlers"
    );
    assert!(
        app.coding_data_retention_opt_out,
        "declining leaves the user opted out"
    );
    assert!(
        app.agents
            .values()
            .all(|a| !matches!(a.active_modal, Some(ActiveModal::Settings { .. }))),
        "[Opt out] answers the question; it must not detour into settings"
    );
}

/// A superseded reply must not touch state. `[Opt out]` fires a write, the
/// user opts in from settings before it lands, and only then does the stale
/// decline answer: its `rollback_to_opted_in: false` was captured before the
/// opt-in existed, so applying it would flip the pager to opted-out while
/// the server holds opted-in — claiming data isn't retained when it is.
#[test]
fn superseded_coding_data_reply_cannot_clobber_a_newer_write() {
    for stale_failed in [true, false] {
        let mut app = privacy_banner_ready_app();

        // Write 1: the banner decline.
        let _ = dispatch(Action::PrivacyBannerOptOut, &mut app);
        assert_eq!(app.coding_data_write_seq, 1);

        // Write 2: the user opts in from settings, and it confirms.
        let _ = dispatch(Action::SetCodingDataSharing { opted_in: true }, &mut app);
        assert_eq!(app.coding_data_write_seq, 2);
        let _ = dispatch(
            Action::TaskComplete(TaskResult::CodingDataSharingUpdated {
                agent_id: AgentId(0),
                opted_in: true,
                seq: 2,
            }),
            &mut app,
        );
        assert!(!app.coding_data_retention_opt_out, "opted in");

        // Write 1 finally answers, either way it can.
        let stale_reply = if stale_failed {
            TaskResult::CodingDataSharingFailed {
                agent_id: AgentId(0),
                error: "network timeout".into(),
                rollback_to_opted_in: false,
                seq: 1,
            }
        } else {
            TaskResult::CodingDataSharingUpdated {
                agent_id: AgentId(0),
                opted_in: false,
                seq: 1,
            }
        };
        let effects = dispatch(Action::TaskComplete(stale_reply), &mut app);

        assert!(effects.is_empty(), "stale reply must emit nothing");
        assert!(
            !app.coding_data_retention_opt_out,
            "stale reply must not undo the newer opt-in (failed={stale_failed})"
        );
        assert!(
            app.agents[&AgentId(0)].toast.is_none(),
            "stale reply must not toast — nothing the user is looking at failed"
        );
    }
}

/// A double-click (or a stale frame's hit rect) must not send a second
/// decline.
#[test]
fn privacy_banner_opt_out_is_idempotent() {
    let mut app = privacy_banner_ready_app();
    let _ = dispatch(Action::PrivacyBannerOptOut, &mut app);
    let again = dispatch(Action::PrivacyBannerOptOut, &mut app);
    assert!(
        again.is_empty(),
        "second dismissal must be inert: {again:?}"
    );
}

#[test]
fn dispatch_rename_session_updates_display_name_locally() {
    let mut app = test_app_with_agent();
    let effects = dispatch_rename_session(&mut app, "renamed via slash".into());
    assert_eq!(effects.len(), 1);
    assert_eq!(
        app.agents[&AgentId(0)].display_name.as_deref(),
        Some("renamed via slash"),
        "/rename must also update local display_name cache"
    );
}

/// `ConfirmResetSetting { choice: Reset }` on a SHARED Bool
/// target restores the Settings modal AND fires the typed
/// `Action::SetCompactMode(default)` via recursive dispatch —
/// the `Effect::PersistSetting` is the externally-observable
/// signal. Also asserts the ui_snapshot was
/// refreshed to the new (post-reset) value (symmetric with the
/// Cancel test's snapshot assertion).
#[test]
fn dispatch_confirm_reset_setting_reset_dispatches_typed_setter_for_shared_bool() {
    use crate::settings::SettingValue;
    use crate::views::modal::{ActiveModal, ResetSettingsResult};
    let mut app = test_app_with_agent();
    // Flip compact_mode to true so we can observe the reset back
    // to its default (false).
    let _ = dispatch(Action::SetCompactMode(true), &mut app);
    assert!(app.current_ui.compact_mode);

    setup_reset_confirm_open(&mut app, "compact_mode");

    let effects = dispatch(
        Action::ConfirmResetSetting {
            choice: ResetSettingsResult::Reset,
        },
        &mut app,
    );

    // Recursive dispatch into Action::SetCompactMode(false) emits
    // the persist effect.
    assert_eq!(effects.len(), 1);
    match &effects[0] {
        Effect::PersistSetting { key, value, .. } => {
            assert_eq!(*key, "compact_mode");
            assert_eq!(value, &SettingValue::Bool(false));
        }
        other => panic!("expected PersistSetting, got {other:?}"),
    }
    // In-memory state is reset to the default.
    assert!(!app.current_ui.compact_mode);
    // Modal is restored AND ui_snapshot reflects the new value
    // (symmetric with the Cancel test).
    let agent = app.agents.get(&AgentId(0)).expect("agent must exist");
    match &agent.active_modal {
        Some(ActiveModal::Settings { state }) => {
            assert!(
                !state.ui_snapshot.compact_mode,
                "ui_snapshot must reflect the post-reset value"
            );
        }
        _ => panic!("Reset branch must restore the Settings modal"),
    }
}

/// `ConfirmResetSetting { choice: Reset }` on a SHARED Enum
/// target (`theme`) dispatches `Action::SetTheme(default)` via
/// recursive dispatch — verifies the action_for_reset Enum arm.
#[test]
fn dispatch_confirm_reset_setting_reset_dispatches_typed_setter_for_shared_enum() {
    use crate::settings::SettingValue;
    use crate::views::modal::ResetSettingsResult;
    // SetTheme mutates the global theme cache — serialize with the
    // other theme tests via the theme test lock.
    with_theme_test_env(|| {
        let mut app = test_app_with_agent();
        // Flip theme to a non-default first.
        let _ = dispatch(Action::SetTheme("tokyonight".to_string()), &mut app);
        assert_eq!(app.current_ui.theme.as_deref(), Some("tokyonight"));

        setup_reset_confirm_open(&mut app, "theme");

        let effects = dispatch(
            Action::ConfirmResetSetting {
                choice: ResetSettingsResult::Reset,
            },
            &mut app,
        );

        // Reset → SetTheme("groknight") (the registered default).
        assert_eq!(effects.len(), 1);
        match &effects[0] {
            Effect::PersistSetting { key, value, .. } => {
                assert_eq!(*key, "theme");
                assert_eq!(value, &SettingValue::Enum("groknight"));
            }
            other => panic!("expected PersistSetting, got {other:?}"),
        }
        assert_eq!(app.current_ui.theme.as_deref(), Some("groknight"));
    });
}

#[test]
fn show_usage_on_welcome_screen_is_noop() {
    let mut app = test_app();
    let effects = dispatch(Action::ShowUsage, &mut app);
    assert!(
        effects.is_empty(),
        "ShowUsage with no active agent should be a no-op"
    );
}

#[test]
fn show_usage_with_redirect_url_fetches_session_only() {
    // Redirect link is deferred until SessionUsageComplete (see billing tests).
    let mut app = test_app_with_agent();
    app.usage_billing_redirect_url = Some("https://billing.example.com/me".to_string());
    let before = agent_scrollback_len(&app);
    let effects = dispatch(Action::ShowUsage, &mut app);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::FetchSessionUsage { agent_id, .. }] if *agent_id == AgentId(0)
        ),
        "got: {effects:?}"
    );
    assert_eq!(agent_scrollback_len(&app), before);
}

// ── Minimal update-notice tests ──────────────────────────────────────

#[test]
fn minimal_update_notice_commits_a_system_block() {
    let mut app = test_app_with_agent();
    let before = agent_scrollback_len(&app);
    commit_minimal_update_notice(&mut app, "9.9.9");
    assert_eq!(agent_scrollback_len(&app), before + 1);
    let text = last_system_text(&app, AgentId(0));
    assert!(text.contains("Update available: v9.9.9"), "got: {text:?}");
    assert!(text.contains("restart to apply"), "got: {text:?}");
}

#[test]
fn minimal_update_notice_no_active_agent_is_noop() {
    let mut app = test_app();
    // Must not panic and must not require an agent.
    commit_minimal_update_notice(&mut app, "9.9.9");
}

// ── Tutorial dispatch tests ──────────────────────────────────────────

/// `/tutorial` (and the palette entry) open the overlay; dispatching again
/// while open toggles it closed. No side effects either way.
#[test]
fn open_tutorial_toggles_overlay_without_effects() {
    let mut app = test_app();
    let effects = dispatch(Action::OpenTutorial, &mut app);
    assert!(app.tutorial.is_some(), "tutorial opens");
    assert!(effects.is_empty(), "open emits nothing, got: {effects:?}");

    let effects = dispatch(Action::OpenTutorial, &mut app);
    assert!(app.tutorial.is_none(), "toggle closes");
    assert!(effects.is_empty(), "close emits nothing, got: {effects:?}");
}
