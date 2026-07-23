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

#[test]
fn show_privacy_info_zdr() {
    let mut app = test_app_with_agent();
    app.is_zdr = true;
    let effects = dispatch(Action::ShowPrivacyInfo, &mut app);
    assert!(effects.is_empty());
    let text = last_system_text(&app, AgentId(0));
    assert!(text.contains("Zero Data Retention"));
    assert!(
        text.contains("Other settings (not changed by /privacy)"),
        "must list other settings knobs: {text}",
    );
    assert!(
        text.contains("GROK_TELEMETRY_ENABLED") && text.contains("GROK_EXTERNAL_OTEL"),
        "must list telemetry/OTEL config keys: {text}",
    );
}

/// `/privacy` info-print uses the desktop-aligned "privacy mode" /
/// "share data" labels from the user's intentional rewrite.
#[test]
fn show_privacy_info_opted_out() {
    let mut app = test_app_with_agent();
    app.coding_data_retention_opt_out = true;
    let effects = dispatch(Action::ShowPrivacyInfo, &mut app);
    assert!(effects.is_empty());
    let text = last_system_text(&app, AgentId(0));
    assert!(
        text.contains("Privacy: privacy mode"),
        "info-print must use 'Privacy: privacy mode' (desktop-aligned label): {text}",
    );
    assert!(text.contains("/privacy opt-in"));
    assert!(
        text.contains("Other settings (not changed by /privacy)")
            && text.contains("GROK_TELEMETRY_ENABLED")
            && text.contains("trace_upload")
            && text.contains("GROK_EXTERNAL_OTEL"),
        "must list config knobs not changed by /privacy: {text}",
    );
}

#[test]
fn show_privacy_info_opted_in() {
    let mut app = test_app_with_agent();
    app.coding_data_retention_opt_out = false;
    let effects = dispatch(Action::ShowPrivacyInfo, &mut app);
    assert!(effects.is_empty());
    let text = last_system_text(&app, AgentId(0));
    assert!(
        text.contains("Privacy: share data"),
        "info-print must use 'Privacy: share data' (desktop-aligned label): {text}",
    );
    assert!(text.contains("/privacy opt-out"));
}

/// The info-print uses desktop-aligned labels ("privacy mode" /
/// "share data"). This test pins those labels to catch accidental
/// regressions to the registry's "Opt in" / "Opt out" display
/// strings.
#[test]
fn show_privacy_info_does_not_use_old_desktop_labels() {
    // opted-out → "Privacy: privacy mode"
    let mut app = test_app_with_agent();
    app.coding_data_retention_opt_out = true;
    let _ = dispatch(Action::ShowPrivacyInfo, &mut app);
    let text = last_system_text(&app, AgentId(0));
    assert!(
        text.contains("privacy mode"),
        "[opted-out] info-print must contain 'privacy mode': {text:?}",
    );

    // opted-in → "Privacy: share data"
    let mut app = test_app_with_agent();
    app.coding_data_retention_opt_out = false;
    let _ = dispatch(Action::ShowPrivacyInfo, &mut app);
    let text = last_system_text(&app, AgentId(0));
    assert!(
        text.contains("share data"),
        "[opted-in] info-print must contain 'share data': {text:?}",
    );
}

// ── coding_data_sharing dispatch tests ───
//
// The dispatcher uses **optimistic + rollback + toast**, matching the
// `set_yolo_mode` pattern. These tests pin the contract:
//   - Guards (ZDR, non-admin team) toast and short-circuit.
//   - Idempotent dispatch toasts but emits no Effect.
//   - Optimistic mutation flips `app.coding_data_retention_opt_out`
//     BEFORE the Effect is emitted.
//   - `Effect::SetCodingDataSharing` carries
//     `rollback_to_opted_in = previous_value`.
//   - `TaskResult::CodingDataSharingFailed` reverts the optimistic
//     mutation; `TaskResult::CodingDataSharingUpdated` re-anchors
//     to the server-confirmed value.

/// Idempotent re-dispatch when already opted-in toasts but emits
/// no Effect (avoids a wasted ACP round-trip).
///
/// Toast uses the **display name** ("Opt in", not the
/// snake-case canonical "opt-in") AND the **destructive `⚠`
/// glyph** on the opt-in direction (privacy-degrading).
#[test]
fn set_coding_data_sharing_idempotent_opt_in() {
    let mut app = test_app_with_agent();
    app.coding_data_retention_opt_out = false; // currently opted-in
    let effects = dispatch(Action::SetCodingDataSharing { opted_in: true }, &mut app);
    assert!(
        effects.is_empty(),
        "idempotent re-dispatch must NOT emit Effect"
    );
    let toast = read_toast(&app);
    assert!(
        toast.contains("Opt in"),
        "toast must show display name 'Opt in' (PR 9 R1, General-3 Issue 6): {toast}",
    );
    assert!(
        !toast.contains("opt-in"),
        "toast must NOT use snake-case canonical 'opt-in' — display name only: {toast}",
    );
    assert!(
        toast.contains('\u{26A0}'),
        "idempotent opt-in toast uses ⚠ destructive-warning glyph (PR 9 R1, \
             General-3 Issue 5): {toast}",
    );
    // State unchanged.
    assert!(
        !app.coding_data_retention_opt_out,
        "idempotent path must not mutate state",
    );
}

/// Idempotent re-dispatch when already opted-out toasts but emits
/// no Effect.
///
/// Opt-out direction uses the **uniform `✓` glyph**
/// (restoring the safe default) and the display name "Opt out".
#[test]
fn set_coding_data_sharing_idempotent_opt_out() {
    let mut app = test_app_with_agent();
    app.coding_data_retention_opt_out = true; // currently opted-out
    let effects = dispatch(Action::SetCodingDataSharing { opted_in: false }, &mut app);
    assert!(
        effects.is_empty(),
        "idempotent re-dispatch must NOT emit Effect"
    );
    let toast = read_toast(&app);
    assert!(
        toast.contains("Opt out"),
        "toast must show display name 'Opt out': {toast}",
    );
    assert!(
        toast.contains('\u{2713}'),
        "idempotent opt-out toast uses ✓ safe-default glyph: {toast}",
    );
    assert!(
        !toast.contains('\u{26A0}'),
        "opt-out is the safe direction — must NOT use ⚠: {toast}",
    );
    // State unchanged.
    assert!(
        app.coding_data_retention_opt_out,
        "idempotent path must not mutate state",
    );
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
/// optimistically AND toasts.
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
        } => {
            assert_eq!(*agent_id, AgentId(0));
            assert!(!*opted_in);
            assert!(
                *rollback_to_opted_in,
                "rollback_to_opted_in must be pre-toggle value (true == opted-in)",
            );
        }
        other => panic!("expected SetCodingDataSharing Effect, got {other:?}"),
    }
    // Optimistic mutation applied.
    assert!(
        app.coding_data_retention_opt_out,
        "dispatch must optimistically mutate state",
    );
    // Toast on every dispatch (SHELL setter contract).
    assert!(app.agents[&AgentId(0)].toast.is_some());
}

/// `TaskResult::CodingDataSharingUpdated` re-anchors state to the
/// server-confirmed value (defense-in-depth) and re-toasts.
#[test]
fn coding_data_sharing_updated_re_anchors_state_and_re_toasts() {
    let mut app = test_app_with_agent();
    // Simulate post-optimistic state: opted-out.
    app.coding_data_retention_opt_out = true;
    let id = AgentId(0);
    // Server confirms opt-out (same as optimistic).
    let effects = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingUpdated {
            agent_id: id,
            opted_in: false,
        }),
        &mut app,
    );
    assert!(effects.is_empty(), "TaskResult arm must NOT emit Effect");
    // State re-anchored (was already true, stays true).
    assert!(app.coding_data_retention_opt_out);
    // Re-toast on confirmation uses display name + ✓.
    let toast = read_toast(&app);
    assert!(
        toast.contains("Opt out"),
        "confirmation toast must use display name 'Opt out': {toast}",
    );
    assert!(
        toast.contains('\u{2713}'),
        "opt-out confirmation toast uses ✓: {toast}",
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
    let effects = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingUpdated {
            agent_id: id,
            opted_in: true, // server says opted-in
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    // State corrected to match server.
    assert!(
        !app.coding_data_retention_opt_out,
        "server-confirmed opt-in must overwrite optimistic opt-out",
    );
    // Server-correction toast uses the destructive ⚠
    // pattern for the opt-in direction (the privacy-degrading
    // override deserves the warning glyph even if the SERVER, not
    // the user, made the call).
    let toast = read_toast(&app);
    assert!(
        toast.contains("Opt in"),
        "post-correction toast uses display name 'Opt in': {toast}",
    );
    assert!(
        toast.contains('\u{26A0}'),
        "opt-in direction always uses ⚠ glyph, even on server-correction path: {toast}",
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
    let effects = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: id,
            error: "server error".into(),
            rollback_to_opted_in: true,
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
    let effects = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: id,
            error: "network timeout".into(),
            rollback_to_opted_in: false,
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
    let _ = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: AgentId(0),
            error: "x".into(),
            rollback_to_opted_in: true,
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

// ── coding_data_sharing toast tests ─────────────

/// The opt-in transition
/// uses the **`⚠` destructive-warning glyph** + spelled-out
/// consequence text — mirroring `yolo_toast`'s
/// "Always-approve ON: all tool actions auto-run" pattern. The
/// consequence text is verbatim-pinned because the toast is the
/// only post-commit feedback for a privacy-degrading transition;
/// a future PR that softens the wording silently degrades the
/// safety affordance.
#[test]
fn set_coding_data_sharing_opt_in_renders_destructive_warning_toast() {
    let mut app = test_app_with_agent();
    app.coding_data_retention_opt_out = true; // currently opted-out
    let effects = dispatch(Action::SetCodingDataSharing { opted_in: true }, &mut app);
    assert_eq!(effects.len(), 1, "non-idempotent opt-in must emit Effect");
    let toast = read_toast(&app);
    assert!(
        toast.contains('\u{26A0}'),
        "opt-in toast MUST use ⚠ glyph (PR 9 R1, General-3 Issue 5 — \
             privacy-degrading transition deserves destructive-warning glyph): {toast}",
    );
    assert!(
        !toast.contains('\u{2713}'),
        "opt-in toast MUST NOT use the uniform ✓ glyph — that's the \
             safe-default toast for opt-out: {toast}",
    );
    assert!(
        toast.contains("Opt in"),
        "destructive toast still uses display name 'Opt in': {toast}",
    );
    // Consequence text pinned: a future PR softening this loses
    // the safety affordance.
    assert!(
        toast.contains("code samples"),
        "destructive toast must spell out the consequence \
             (mention 'code samples'): {toast}",
    );
    assert!(
        toast.contains("training"),
        "destructive toast must spell out the consequence \
             (mention 'training'): {toast}",
    );
}

/// The opt-out transition uses the
/// uniform `✓` glyph (safe default), NOT the destructive `⚠`.
/// Mirrors `yolo_toast(false)` precedent — restoring the safe
/// default doesn't warrant the heavier visual.
#[test]
fn set_coding_data_sharing_opt_out_renders_safe_default_toast() {
    let mut app = test_app_with_agent();
    app.coding_data_retention_opt_out = false; // currently opted-in
    let _ = dispatch(Action::SetCodingDataSharing { opted_in: false }, &mut app);
    let toast = read_toast(&app);
    assert!(
        toast.contains('\u{2713}'),
        "opt-out toast uses ✓ safe-default glyph: {toast}",
    );
    assert!(
        !toast.contains('\u{26A0}'),
        "opt-out toast MUST NOT use ⚠ — that's reserved for the privacy-degrading \
             direction (PR 9 R1): {toast}",
    );
    assert!(toast.contains("Opt out"));
}

/// The toast renders
/// the registered `EnumChoice.display` ("Opt in" / "Opt out"),
/// NOT the persisted canonical ("opt-in" / "opt-out"). Mirrors
/// the `set_theme_toast_format_uses_display_name` contract.
/// The display strings here are pinned by the
/// `coding_data_sharing_choices_use_canonical_strings` e2e test
/// (registry side) AND
/// `pr9_coding_data_sharing_choices_use_canonical_strings` (which
/// also pins the display labels via the same EnumChoice
/// entries).
#[test]
fn coding_data_sharing_toast_format_uses_display_name() {
    let mut app = test_app_with_agent();
    // Opt-in direction.
    app.coding_data_retention_opt_out = true;
    let _ = dispatch(Action::SetCodingDataSharing { opted_in: true }, &mut app);
    let opt_in_toast = read_toast(&app);
    assert!(
        opt_in_toast.contains("Opt in"),
        "opt-in toast uses display 'Opt in', not canonical 'opt-in': {opt_in_toast}",
    );
    // Clear and test opt-out direction.
    app.agents.get_mut(&AgentId(0)).unwrap().toast = None;
    app.coding_data_retention_opt_out = false;
    let _ = dispatch(Action::SetCodingDataSharing { opted_in: false }, &mut app);
    let opt_out_toast = read_toast(&app);
    assert!(
        opt_out_toast.contains("Opt out"),
        "opt-out toast uses display 'Opt out', not canonical 'opt-out': {opt_out_toast}",
    );
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
    let _ = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: id,
            error: huge_error.clone(),
            rollback_to_opted_in: false,
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
    let _ = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: id,
            error: multiline.clone(),
            rollback_to_opted_in: false,
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
    let _ = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: id,
            error: short_clean.clone(),
            rollback_to_opted_in: false,
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
    app.privacy_banner_accept_inflight = false;
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

/// Accept success: ACP confirmation acks the banner.
#[test]
fn privacy_banner_accept_success_acks() {
    let mut app = privacy_banner_ready_app();
    let effects = dispatch(Action::PrivacyBannerAccept, &mut app);
    assert_eq!(effects.len(), 1);
    assert!(matches!(
        &effects[0],
        Effect::SetCodingDataSharing { opted_in: true, .. }
    ));
    assert!(app.privacy_banner_accept_inflight);
    assert!(!app.coding_data_retention_opt_out);
    assert!(app.privacy_banner_acked.is_none());

    let ack_effects = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingUpdated {
            agent_id: AgentId(0),
            opted_in: true,
        }),
        &mut app,
    );
    assert!(!app.privacy_banner_accept_inflight);
    assert!(app.privacy_banner_acked.is_some());
    assert!(
        ack_effects
            .iter()
            .any(|e| matches!(e, Effect::PersistPrivacyBannerAcked { .. })),
        "success must persist ack: {ack_effects:?}"
    );
}

/// Accept failure: no ack; welcome toast carries the error.
#[test]
fn privacy_banner_accept_failure_no_ack_sets_welcome_toast() {
    let mut app = privacy_banner_ready_app();
    let effects = dispatch(Action::PrivacyBannerAccept, &mut app);
    assert_eq!(effects.len(), 1);
    assert!(app.privacy_banner_accept_inflight);

    let fail_effects = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: AgentId(0),
            error: "server error".into(),
            rollback_to_opted_in: false,
        }),
        &mut app,
    );
    assert!(fail_effects.is_empty());
    assert!(!app.privacy_banner_accept_inflight);
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
        "welcome toast on Accept failure: {toast}"
    );
    assert!(toast.contains("server error"), "error in toast: {toast}");
}

/// Customize while an Accept ACP call is inflight must be a no-op: an
/// eager ack would survive the Accept-failure rollback and hide the
/// banner forever.
#[test]
fn privacy_banner_customize_noop_while_accept_inflight() {
    let mut app = privacy_banner_ready_app();
    let _ = dispatch(Action::PrivacyBannerAccept, &mut app);
    assert!(app.privacy_banner_accept_inflight);

    let effects = dispatch(Action::PrivacyBannerCustomize, &mut app);
    assert!(
        effects.is_empty(),
        "customize during inflight accept must be a no-op: {effects:?}"
    );
    assert!(app.privacy_banner_acked.is_none(), "no ack while inflight");

    let _ = dispatch(
        Action::TaskComplete(TaskResult::CodingDataSharingFailed {
            agent_id: AgentId(0),
            error: "server error".into(),
            rollback_to_opted_in: false,
        }),
        &mut app,
    );
    assert!(
        app.privacy_banner_should_show(),
        "failed Accept must keep the banner even after a raced Customize"
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
