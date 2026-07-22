use super::*;
use serde::Deserialize;

/// Handle `x.ai/models/update` — model list changed (etag-triggered refresh).
pub(super) fn handle_models_update(notif: &acp::ExtNotification, app: &mut AppView) -> bool {
    if let Ok(model_state) = serde_json::from_str::<acp::SessionModelState>(notif.params.get()) {
        use crate::acp::model_state::ModelState;
        let mut new_models = ModelState::from(Some(model_state));
        // Model refreshes replace the native catalog. Preserve provider models
        // added by the ACP router (e.g. Codex subscription models) so `/model`
        // does not lose them mid-run.
        for (id, info) in &app.models.available {
            if info
                .meta
                .as_ref()
                .and_then(|meta| meta.get("provider"))
                .and_then(serde_json::Value::as_str)
                == Some("codex")
            {
                new_models.available.insert(id.clone(), info.clone());
            }
        }
        tracing::info!(
            count = new_models.available.len(),
            "models updated via x.ai/models/update"
        );

        let shell_fallback_current = new_models.current.clone();

        // Override app-level default with the active agent's model.
        let mut app_models = new_models.clone();
        if let ActiveView::Agent(id) = app.active_view
            && let Some(agent) = app.agents.get(&id)
            && let Some(ref agent_model) = agent.session.models.current
            && app_models.available.contains_key(agent_model)
        {
            app_models.current = Some(agent_model.clone());
        }

        app.models = app_models;

        for agent in app.agents.values_mut() {
            // Log when an update drops the agent's active model — this is the
            // moment the status bar visibly "switches model mid-conversation"
            // (the agent falls back to the shell's current model below).
            if let Some(ref current) = agent.session.models.current
                && !new_models.available.contains_key(current)
            {
                tracing::warn!(
                    current_model = %current.0,
                    fallback = ?shell_fallback_current.as_ref().map(|m| m.0.as_ref()),
                    available_count = new_models.available.len(),
                    "models update removed this agent's current model; falling back"
                );
            }
            agent
                .session
                .models
                .update_catalog(new_models.available.clone(), shell_fallback_current.clone());
        }
        true
    } else {
        tracing::warn!("Failed to parse x.ai/models/update");
        false
    }
}

/// Handle `x.ai/settings/update` — remote settings refreshed on `/new`.
pub(super) fn handle_settings_update(notif: &acp::ExtNotification, app: &mut AppView) -> bool {
    let Ok(update) = serde_json::from_str::<PagerSettingsUpdate>(notif.params.get()) else {
        tracing::warn!("Failed to parse x.ai/settings/update");
        return false;
    };

    if let Some(v) = update.auto_permission_mode_enabled {
        // Keep the pager's auto-permission-mode gate live with the remote settings
        // remote tier (the leader caches it agent-side; the pager process needs
        // its own copy). Refresh the startup snapshot so the Shift+Tab cycle and
        // the settings modal both reflect a remote-only enablement/kill-switch
        // without a restart.
        xai_grok_shell::util::config::cache_remote_auto_permission_mode_enabled(Some(v));
        app.auto_mode_gate = xai_grok_shell::util::config::auto_permission_mode_enabled_from_disk();
        // Mid-session kill switch: when the gate just went off, drop displayed
        // Auto to Ask + clear every agent's per-session flag (shared with the
        // startup reconcile), AND tell live sessions to leave Auto. Clearing only
        // the display would let the agent keep classifier-approving while the UI
        // shows "Ask" — the emergency-off must actually disable enforcement.
        if !app.auto_mode_gate {
            // Sessions to notify: agents that HAD Auto on (capture before the
            // downgrade clears the flag) and have a live session id.
            let leaving_auto: Vec<acp::SessionId> = app
                .agents
                .values()
                .filter(|a| a.session.is_auto())
                .filter_map(|a| a.session.session_id.clone())
                .collect();
            super::super::dispatch::downgrade_displayed_auto_if_gated(app);
            notify_sessions_leave_auto(app, &leaving_auto);
        }
        // Reveal/hide `/auto` on every slash surface in lockstep with the gate
        // (covers both a mid-session kill-switch and re-enablement).
        app.sync_permission_mode_slash_gate();
    }

    // `permission_mode` is presence-aware (omit / null / string). While the
    // soft default still owns the mode, a push re-arms `default_yolo` + UI for
    // the next `/new`; once the user claims a mode (Shift+Tab / settings /
    // `/mode`) the latch is cleared and pushes leave it alone.
    if let Some(remote_opt) = update.permission_mode.as_ref()
        && app.permission_mode_from_soft_default
    {
        // One config read at the I/O boundary; the applier is deterministic.
        let root = xai_grok_shell::config::load_effective_config().ok();
        apply_soft_default_permission_mode(
            app,
            root.as_ref().and_then(|r| r.get("ui")),
            remote_opt.as_deref(),
        );
    }

    if let Some(v) = update.show_resolved_model {
        app.show_resolved_model = v;
    }
    if let Some(v) = update.sharing_enabled {
        app.sharing_enabled = v;
        // Propagate to existing agents so slash-command registries stay
        // in sync (same fan-out pattern used when creating new agents).
        for agent in app.agents.values_mut() {
            agent.set_sharing_enabled(v);
        }
    }
    // Tier before voice: same payload may set "API Key" and voice_mode_enabled=false.
    // Always recompute is_api_key_auth from the tier so a later Free/SuperGrok
    // stamp does not leave API-key bypass / a hidden billing surface stuck.
    if let Some(v) = update.subscription_tier_display {
        let was_api_key = app.is_api_key_auth;
        let is_key = super::super::app_view::is_api_key_label(&v);
        app.is_api_key_auth = is_key;
        app.usage_visible = !is_key && app.team_name.is_none();
        app.sync_billing_surface_to_agents();
        app.subscription_tier = Some(v);
        app.apply_tier_restrictions();
        // Leaving API Key → free/X Basic without a voice field: drop force-on.
        // Paid tiers keep voice; remote settings may send voice_mode_enabled later.
        if was_api_key
            && !is_key
            && update.voice_mode_enabled.is_none()
            && app
                .subscription_tier
                .as_deref()
                .is_some_and(xai_grok_shell::tier::is_restricted_tier_name)
        {
            app.voice_reset();
            app.voice_ui_active = false;
            app.apply_voice_mode_enabled(false);
        }
    }
    if let Some(remote_v) = update.voice_mode_enabled {
        let v = crate::app::resolve_voice_mode_live(Some(remote_v), app.is_api_key_auth);
        if !v {
            app.voice_reset();
            app.voice_ui_active = false;
        }
        app.apply_voice_mode_enabled(v);
    } else {
        app.ensure_voice_for_api_key();
    }
    // TODO: extract resolve_session_picker_grouped helper (duplicates event_loop.rs:143-160)
    // Respect env var > config > remote precedence (mirrors event_loop.rs startup).
    if let Some(remote_val) = update.session_picker_grouped {
        let resolved = std::env::var("GROK_SESSION_PICKER_GROUPED")
            .ok()
            .and_then(|v| match v.as_str() {
                "1" | "true" => Some(true),
                "0" | "false" => Some(false),
                _ => None,
            })
            .or_else(|| {
                xai_grok_shell::config::load_effective_config()
                    .ok()
                    .and_then(|cfg| cfg.get("cli")?.get("session_picker_grouped")?.as_bool())
            })
            .unwrap_or(remote_val);
        app.session_picker_grouped = resolved;
    }
    if let Some(v) = update.subscription_watch_interval_secs {
        app.subscription_watch_interval_secs = Some(v);
    }

    // Gate update logic:
    // - allow_access == Some(true): explicitly granted → lift the gate
    // - gate_message.is_some(): server sent a new message → impose/update
    // - Neither condition met: don't touch the gate. In particular,
    //   allow_access=Some(false) without a gate_message must NOT clear the
    //   gate (gate_from_settings returns None when gate_message is absent,
    //   which would incorrectly lift an existing gate).
    if update.allow_access == Some(true) {
        let effs = app.lift_gate();
        app.pending_effects.extend(effs);
    } else if let Some(msg) = update.gate_message.as_ref()
        && !msg.is_empty()
    {
        // (An empty gate_message would only clear the gate message text, NOT
        // access, so it intentionally does not touch the gate here.)
        let effs = app.impose_gate(xai_grok_shell::auth::GateInfo {
            message: msg.clone(),
            url: update.gate_url.clone(),
            label: update.gate_label.clone(),
        });
        app.pending_effects.extend(effs);
    }

    // Load config layers once for tips + group_tool_verbs +
    // collapsed_edit_blocks resolution. Loaded unconditionally: the UI flags
    // re-resolve on every update (see below), and updates are rare (post-auth
    // refresh, `/new`), so three small TOML reads are fine.
    let (requirements, user_config, managed_config) = (
        xai_grok_shell::config::load_merged_requirements(),
        xai_grok_shell::config::load_from_disk().ok(),
        xai_grok_shell::config::load_managed_config().ok(),
    );

    // Local layers may beat remote — re-resolve the full chain into the render
    // cache (mirrors the event_loop.rs startup resolve). Runs on None too: the
    // shell always publishes this field from its live remote tier, so None
    // means remote settings cleared it (or an older shell that cannot deliver the
    // remote tier at all) — either way resolving without a remote value is
    // correct, and it reverts a previously cached remote enable back to the
    // local/default (off) resolution instead of leaving Some(true) stuck
    // until restart.
    let remote = xai_grok_shell::util::config::RemoteSettings {
        group_tool_verbs: update.group_tool_verbs,
        ..Default::default()
    };
    let resolved = xai_grok_shell::util::config::resolve_group_tool_verbs(
        requirements.as_ref(),
        user_config.as_ref(),
        managed_config.as_ref(),
        Some(&remote),
    )
    .value;
    // On a real flip, re-fold every live transcript (mirrors dispatch's
    // set_group_tool_verbs_inner); unchanged values keep `/new` cheap.
    // Stale expansion ids describe the old grouping shape — drop them so the
    // re-fold can't reopen a verb slot expanded or mark a coincident dense
    // group expanded (see `clear_group_expansion`).
    if resolved != crate::appearance::cache::load_group_tool_verbs() {
        crate::appearance::cache::set_group_tool_verbs(resolved);
        for agent in app.agents.values_mut() {
            agent.scrollback.clear_group_expansion();
            agent.scrollback.invalidate_heights();
            for child in agent.subagent_views.values_mut() {
                child.scrollback.clear_group_expansion();
                child.scrollback.invalidate_heights();
            }
        }
    }

    // Same None-reverts contract as group_tool_verbs above: re-resolve the
    // full local chain with the pushed remote tier so a cleared remote settings
    // field falls back to local/default instead of staying latched.
    let remote = xai_grok_shell::util::config::RemoteSettings {
        collapsed_edit_blocks: update.collapsed_edit_blocks,
        ..Default::default()
    };
    let resolved = xai_grok_shell::util::config::resolve_collapsed_edit_blocks(
        requirements.as_ref(),
        user_config.as_ref(),
        managed_config.as_ref(),
        Some(&remote),
    )
    .value;
    // On a real flip, re-materialize on-default Edit rows + repaint suffixes
    // in every live transcript (mirrors dispatch's
    // set_collapsed_edit_blocks_inner); unchanged values keep `/new` cheap.
    let prev = crate::appearance::cache::load_collapsed_edit_blocks();
    if resolved != prev {
        crate::appearance::cache::set_collapsed_edit_blocks(resolved);
        for agent in app.agents.values_mut() {
            agent
                .scrollback
                .apply_collapsed_edit_blocks_flip(prev, resolved);
            for child in agent.subagent_views.values_mut() {
                child
                    .scrollback
                    .apply_collapsed_edit_blocks_flip(prev, resolved);
            }
        }
    }

    // Re-resolve tips from config layers + the updated remote tips.
    if let Some(remote_tips) = update.tips {
        use xai_grok_shell::util::config::resolve_tips;

        app.tips = resolve_tips(
            requirements.as_ref(),
            user_config.as_ref(),
            managed_config.as_ref(),
            Some(&remote_tips),
        );
        if !app.tips.is_empty() {
            let grok_home = xai_grok_tools::util::grok_home::grok_home();
            app.tip = xai_grok_shell::util::tips::pick_and_advance(&app.tips, &grok_home);
        } else {
            app.tip = None;
        }
    }

    tracing::info!("settings updated via x.ai/settings/update");
    true
}

/// Re-arm the soft-defaulted launch mode from a pushed `permission_mode`
/// (TOML `[ui]` > remote > Ask), for the next `/new` only — live sessions are
/// untouched and nothing is persisted. `effective_ui` is injected so the
/// resolve is deterministic under test. Enforcement gating reuses the app's
/// startup snapshots (`yolo_policy_block`, `auto_mode_gate`); the agent's
/// permission manager re-clamps authoritatively at decision time.
pub(super) fn apply_soft_default_permission_mode(
    app: &mut AppView,
    effective_ui: Option<&toml::Value>,
    remote: Option<&str>,
) {
    let mode = xai_grok_shell::util::config::resolve_permission_mode(effective_ui, remote);
    app.default_yolo = mode.is_always_approve() && app.yolo_policy_block.is_none();
    let auto = mode.is_auto() && app.auto_mode_gate && !app.default_yolo;
    app.current_ui.permission_mode = Some(if auto {
        "auto".to_string()
    } else if app.default_yolo {
        "always-approve".to_string()
    } else {
        xai_grok_shell::util::config::resolved_display_permission_mode(effective_ui, remote)
            .to_string()
    });
}

/// Tell live sessions to leave Auto on the mid-session kill-switch: fire the
/// `x.ai/yolo_mode_changed` notification the agent maps to
/// `SetAutoMode { enabled: false }`, fire-and-forget over the shared ACP channel.
/// The notification is CLIENT-scoped (the agent applies it to every session of
/// the sending client), so one send covers all affected sessions. `yolo_mode` is
/// deliberately OMITTED — the agent skips the yolo branch when the key is absent,
/// so a sibling tab's always-approve is preserved; only auto is cleared.
pub(super) fn notify_sessions_leave_auto(app: &AppView, session_ids: &[acp::SessionId]) {
    if session_ids.is_empty() {
        return;
    }
    let params = serde_json::json!({
        "auto_mode": false,
        "permission_mode": "ask",
    });
    let notification = acp::ExtNotification::new(
        "x.ai/yolo_mode_changed",
        serde_json::value::to_raw_value(&params)
            .expect("serialize yolo_mode_changed params")
            .into(),
    );
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    let args = xai_acp_lib::AcpArgs {
        request: notification,
        response_tx,
    };
    let _ = app.acp_tx.send(args.into());
}

/// Handle `x.ai/sessions/changed` — the leader broadcasts roster
/// upserts/removals to all clients (FleetView dashboard).
pub(super) fn handle_sessions_changed(notif: &acp::ExtNotification, app: &mut AppView) -> bool {
    let Ok(changed) = serde_json::from_str::<crate::app::roster::RosterChanged>(notif.params.get())
    else {
        tracing::warn!("Failed to parse x.ai/sessions/changed");
        return false;
    };
    let mut affected = false;
    for entry in changed.upserted {
        app.upsert_roster_entry(entry);
        affected = true;
    }
    for sid in changed.removed {
        app.remove_roster_entry(&sid);
        affected = true;
    }
    affected
}

pub(super) fn handle_announcements_update(notif: &acp::ExtNotification, app: &mut AppView) -> bool {
    let Ok(parsed) =
        serde_json::from_str::<xai_grok_announcements::AnnouncementsRefreshed>(notif.params.get())
    else {
        return false;
    };

    if parsed.r#gen <= app.announcements_last_gen {
        return false;
    }

    // Re-merge config layers like startup (and the pre-unification settings
    // branch) did: the push carries the remote list only, and a wholesale
    // replace would drop requirements/user/managed announcements and let the
    // prune erase their persisted hide keys. Same disk reads the settings
    // branch performed; pushes are rare.
    let requirements = xai_grok_shell::config::load_merged_requirements();
    let user_config = xai_grok_shell::config::load_from_disk().ok();
    let managed_config = xai_grok_shell::config::load_managed_config().ok();
    apply_announcements_update(
        app,
        parsed.r#gen,
        &parsed.announcements,
        requirements.as_ref(),
        user_config.as_ref(),
        managed_config.as_ref(),
    );
    true
}

/// Apply half of [`handle_announcements_update`], with config layers injected
/// so the merge/prune behavior is unit-testable without disk state.
/// `resolve_announcements` honors `GROK_ANNOUNCEMENTS_OVERRIDE` first, so a
/// backend push can't reintroduce announcements when the override is set.
pub(super) fn apply_announcements_update(
    app: &mut AppView,
    next_gen: u64,
    remote: &[xai_grok_announcements::RemoteAnnouncement],
    requirements: Option<&toml::Value>,
    user_config: Option<&toml::Value>,
    managed_config: Option<&toml::Value>,
) {
    let merged = xai_grok_shell::util::config::resolve_announcements(
        requirements,
        user_config,
        managed_config,
        Some(remote),
    );
    let announcements = xai_grok_announcements::filter_expired(merged);

    app.announcement = match app.announcement.as_ref() {
        Some(current) => announcements
            .iter()
            .find(|a| *a == current)
            .cloned()
            .or_else(|| pick_random_announcement(&announcements)),
        None => pick_random_announcement(&announcements),
    };
    app.active_announcements = announcements;
    app.announcements_last_gen = next_gen;
    // Opportunistic per-ID prune on a real update (never per frame) so the hidden set cannot grow unboundedly.
    if xai_grok_announcements::prune_hidden_announcement_ids(
        &mut app.hidden_announcement_ids,
        &app.active_announcements,
    ) {
        app.pending_effects
            .push(Effect::PersistAnnouncementsHidden {
                hidden_ids: app.hidden_announcement_ids.clone(),
            });
    }
    app.sync_session_announcement_slash_gate();
}

pub(super) fn pick_random_announcement(
    announcements: &[xai_grok_announcements::RemoteAnnouncement],
) -> Option<xai_grok_announcements::RemoteAnnouncement> {
    if announcements.is_empty() {
        return None;
    }
    use rand::Rng;
    let idx = rand::rng().random_range(0..announcements.len());
    announcements.get(idx).cloned()
}

/// Deserialization type for the `x.ai/settings/update` notification payload.
///
/// This is intentionally a separate struct from `SettingsUpdateNotification` in
/// `xai-grok-shell/src/agent/mvp_agent.rs`. The shell side derives `Serialize`
/// and owns the canonical field set from `RemoteSettings`; this pager side
/// derives `Deserialize` and selectively consumes only the fields relevant to
/// the TUI. Keeping them separate avoids coupling the pager to shell internals
/// and lets each side evolve independently (e.g. adding a shell-only field
/// doesn't require a pager change). All fields are `Option` with
/// `#[serde(default)]` so that partial updates and forward-compatible additions
/// are handled gracefully.
///
/// **Keep in sync** with field names/types in `SettingsUpdateNotification` at
/// `xai-grok-shell/src/agent/mvp_agent.rs` when adding fields that both sides
/// need.
#[derive(serde::Deserialize)]
pub(super) struct PagerSettingsUpdate {
    #[serde(default)]
    show_resolved_model: Option<bool>,
    #[serde(default)]
    sharing_enabled: Option<bool>,
    #[serde(default)]
    voice_mode_enabled: Option<bool>,
    #[serde(default)]
    session_picker_grouped: Option<bool>,
    #[serde(default)]
    tips: Option<Vec<String>>,
    // `announcements` is deliberately NOT consumed here: every shell writer of
    // remote_settings also emits gen-ordered `x.ai/announcements/update`
    // (emit_announcements_if_changed), and a gen-less apply on this path could
    // clobber a newer push. Single ingest path: handle_announcements_update.
    #[serde(default)]
    gate_message: Option<String>,
    #[serde(default)]
    gate_url: Option<String>,
    #[serde(default)]
    gate_label: Option<String>,
    #[serde(default)]
    allow_access: Option<bool>,
    #[serde(default)]
    subscription_tier_display: Option<String>,
    #[serde(default)]
    auto_permission_mode_enabled: Option<bool>,
    /// Soft-default permission mode. Presence-aware: omit = no update,
    /// `null` = recompute with remote=None, string = that soft-default.
    /// Omission happens with older shells that predate the field (they can
    /// never clear a mode they don't know about) — that version skew is why
    /// this is tri-state instead of a plain `Option`.
    #[serde(default, deserialize_with = "deserialize_presence_aware_string")]
    permission_mode: Option<Option<String>>,
    #[serde(default)]
    group_tool_verbs: Option<bool>,
    #[serde(default)]
    collapsed_edit_blocks: Option<bool>,
    #[serde(default)]
    subscription_watch_interval_secs: Option<u64>,
}

/// Presence-aware string: omit → `None` (`#[serde(default)]`), null →
/// `Some(None)`, string → `Some(Some(_))`.
fn deserialize_presence_aware_string<'de, D>(
    deserializer: D,
) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::<String>::deserialize(deserializer)?))
}

#[cfg(test)]
mod presence_aware_dto_tests {
    use super::*;

    #[derive(Deserialize)]
    struct Probe {
        #[serde(default, deserialize_with = "deserialize_presence_aware_string")]
        permission_mode: Option<Option<String>>,
    }

    #[test]
    fn permission_mode_dto_distinguishes_omit_from_null() {
        let omit: Probe = serde_json::from_value(serde_json::json!({
            "show_resolved_model": true,
        }))
        .unwrap();
        assert_eq!(omit.permission_mode, None, "omit must be None (no update)");

        let null_v: Probe = serde_json::from_value(serde_json::json!({
            "permission_mode": null,
        }))
        .unwrap();
        assert_eq!(
            null_v.permission_mode,
            Some(None),
            "explicit null must be Some(None)"
        );

        let some_v: Probe = serde_json::from_value(serde_json::json!({
            "permission_mode": "always-approve",
        }))
        .unwrap();
        assert_eq!(
            some_v.permission_mode,
            Some(Some("always-approve".into())),
            "string must be Some(Some(_))"
        );
    }
}
