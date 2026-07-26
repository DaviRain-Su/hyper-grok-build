//! External-OTEL emission gate.
//!
//! Single owner of the fail-closed gate that decides whether customer-owned
//! OTEL telemetry may ship, over the process-global flag in
//! [`xai_grok_telemetry::external`]:
//!
//! 1. Startup (no leader instance yet): [`suppress`] closes the gate before
//!    telemetry init; [`open_at_startup`] re-opens it only for a pure
//!    env-API-key leader ([`should_open_at_startup`]), which has no remote
//!    policy to fetch.
//! 2. Post-auth/refresh (per-leader): [`OtelGate::resolve`] drives the gate
//!    from the [`SettingsFetch`] outcome for the still-live identity.
//!
//! A leader that never authenticates keeps the gate closed for life: the gate
//! fails safe by dropping telemetry, never by shipping it early.

use crate::remote::SettingsFetch;
use crate::util::config::RemoteSettings;

/// Closes the gate. Process-global and idempotent; callable before any
pub(crate) fn suppress() {
    xai_grok_telemetry::external::suppress_external_otel_until_settings();
}

/// Inputs to [`should_open_at_startup`]. Named fields prevent transposed
pub(crate) struct StartupGate {
    pub(crate) has_session: bool,
    pub(crate) has_api_key_env: bool,
    pub(crate) session_pending: bool,
    /// When false, no remote fleet policy can arrive, so the gate fails open to the leader's local telemetry decision.
    pub(crate) remote_fetch_enabled: bool,
}

/// Returns whether a leader opens the gate at startup: only a pure
pub(crate) fn should_open_at_startup(gate: StartupGate) -> bool {
    // No remote policy will arrive with `remote_fetch` off, so fail open to
    if !gate.remote_fetch_enabled {
        return true;
    }
    !gate.has_session && gate.has_api_key_env && !gate.session_pending
}

/// Returns whether a session-less startup is about to mint a grok.com session
pub(crate) fn is_session_pending(
    has_session: bool,
    grok_com_config: &crate::auth::GrokComConfig,
) -> bool {
    !has_session
        && (grok_com_config.auth_provider_command.is_some()
            || crate::auth::devbox_login::is_devbox_environment())
}

/// Opens the gate at startup once [`should_open_at_startup`] holds; a later session re-resolves via [`OtelGate::resolve`].
pub(crate) fn open_at_startup() {
    xai_grok_telemetry::external::mark_external_otel_settings_resolved();
}

/// Per-leader memory over the process-global external-OTEL gate: the credential
#[derive(Default)]
pub(crate) struct OtelGate {
    resolved_for: std::cell::RefCell<Option<String>>,
}

impl OtelGate {
    /// Re-closes the gate before fetching a different identity's policy, so a stale open can't leak across an account switch.
    pub(crate) fn rearm_on_switch(&self, identity: &str) {
        if identity.is_empty() || self.resolved_for.borrow().as_deref() != Some(identity) {
            xai_grok_telemetry::external::suppress_external_otel_until_settings();
        }
    }

    /// Drives the gate from a settings-fetch `outcome` for `identity`: fail-closed on transient outcomes, opens on a definitive one. Returns settings only when fetched.
    pub(crate) fn resolve(
        &self,
        identity: &str,
        outcome: SettingsFetch,
        live_identity: Option<&str>,
    ) -> Option<RemoteSettings> {
        if live_identity != Some(identity) {
            return None;
        }
        match outcome {
            SettingsFetch::Fetched(settings) => {
                self.apply_and_open(identity, Some(&settings));
                Some(*settings)
            }
            SettingsFetch::Rejected => {
                self.apply_and_open(identity, None);
                None
            }
            SettingsFetch::Retry => None,
        }
    }

    /// Applies the tighten-only fleet policy from `settings` (`None` on a `401`), then opens the gate and records `identity` (policy before open).
    fn apply_and_open(&self, identity: &str, settings: Option<&RemoteSettings>) {
        crate::agent::config::apply_external_otel_remote_policy(settings);
        xai_grok_telemetry::external::mark_external_otel_settings_resolved();
        *self.resolved_for.borrow_mut() = Some(identity.to_owned());
    }

    #[cfg(test)]
    pub(crate) fn set_resolved_for(&self, identity: &str) {
        *self.resolved_for.borrow_mut() = Some(identity.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_telemetry::external::{
        is_settings_gate_open, mark_external_otel_settings_resolved,
        suppress_external_otel_until_settings,
    };

    /// Restore the process-global gate open on exit so a closed gate never leaks.
    struct RestoreGate;
    impl Drop for RestoreGate {
        fn drop(&mut self) {
            mark_external_otel_settings_resolved();
        }
    }

    fn fetched() -> SettingsFetch {
        SettingsFetch::Fetched(Box::default())
    }

    #[test]
    fn startup_gate_fails_open_when_remote_fetch_disabled() {
        // remote_fetch off => no remote policy will ever arrive => fail open,
        assert!(should_open_at_startup(StartupGate {
            has_session: true,
            has_api_key_env: false,
            session_pending: false,
            remote_fetch_enabled: false,
        }));
        assert!(!should_open_at_startup(StartupGate {
            has_session: true,
            has_api_key_env: false,
            session_pending: false,
            remote_fetch_enabled: true,
        }));
    }

    #[test]
    #[serial_test::serial]
    fn resolve_opens_only_on_definitive_outcome_for_live_identity() {
        let _restore = RestoreGate;
        let gate = OtelGate::default();

        suppress_external_otel_until_settings();
        assert!(
            gate.resolve("alice", SettingsFetch::Retry, Some("alice"))
                .is_none()
        );
        assert!(
            !is_settings_gate_open(),
            "a transient outcome stays fail-closed"
        );

        assert!(
            gate.resolve("alice", SettingsFetch::Rejected, Some("alice"))
                .is_none()
        );
        assert!(
            is_settings_gate_open(),
            "a rejected credential opens the gate"
        );

        suppress_external_otel_until_settings();
        assert!(gate.resolve("alice", fetched(), Some("alice")).is_some());
        assert!(
            is_settings_gate_open(),
            "a fetched outcome opens for the live identity"
        );
    }

    #[test]
    #[serial_test::serial]
    fn resolve_skips_open_for_a_stale_identity() {
        let _restore = RestoreGate;
        let gate = OtelGate::default();

        suppress_external_otel_until_settings();
        assert!(
            gate.resolve("alice", fetched(), Some("bob")).is_none(),
            "a stale identity must not return settings"
        );
        assert!(
            !is_settings_gate_open(),
            "a stale identity must not open the gate"
        );
    }

    #[test]
    #[serial_test::serial]
    fn rearm_re_closes_for_an_empty_identity() {
        let _restore = RestoreGate;
        let gate = OtelGate::default();

        gate.set_resolved_for("");
        mark_external_otel_settings_resolved();
        gate.rearm_on_switch("");
        assert!(
            !is_settings_gate_open(),
            "an empty identity must always re-close (cannot prove same credential)"
        );
    }
}
