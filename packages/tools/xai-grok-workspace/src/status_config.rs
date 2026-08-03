//! Runtime-tunable timing/threshold config for the workspace tool server.
//!
//! All values are read once at startup from `GROK_WORKSPACE_*` environment
//! variables via [`StatusConfig::from_env`]. Unset or unparseable variables
//! fall back to the documented defaults (with a `warn!` on parse failure), so
//! construction never fails.

use std::str::FromStr;
use std::time::Duration;

// ── Default timing/threshold values ──────────────────────────────────────
// Single source of truth for the `StatusConfig::default()` values and the
// documented fallbacks for each `GROK_WORKSPACE_*` env var.

/// Default interval between status/heartbeat emissions.
const DEFAULT_HEARTBEAT_SECS: u64 = 30;
/// Default transport keepalive interval. Should exceed `heartbeat`.
const DEFAULT_KEEPALIVE_SECS: u64 = 60;
/// Default WebSocket keepalive ping cadence for the server SDK connection.
const DEFAULT_WS_PING_SECS: u64 = 30;
/// Default number of consecutive hub reconnect failures before warning.
const DEFAULT_HUB_WARN_THRESHOLD: u32 = 5;
/// Default base delay (ms) for exponential backoff on failed hub sends.
const DEFAULT_HUB_BACKOFF_BASE_MS: u64 = 100;
/// Default idle window (s) after which an inactive session is pruned. Kept
/// aligned with the sandbox service's idle-hibernate grace.
const DEFAULT_SESSION_IDLE_PRUNE_SECS: u64 = 1800;
/// Default max time (s) to wait for in-flight work to drain on shutdown.
const DEFAULT_DRAIN_TIMEOUT_SECS: u64 = 30;
/// Default per-call timeout (s) for agent gRPC RPCs.
const DEFAULT_AGENT_RPC_TIMEOUT_SECS: u64 = 30;
/// Default timeout (s) for establishing an agent connection.
const DEFAULT_AGENT_CONNECT_TIMEOUT_SECS: u64 = 5;
/// Default preview-activity withhold window. Sourced from the tracker's
/// `PREVIEW_ACTIVITY_WINDOW_MS` so the two can't drift.
const DEFAULT_PREVIEW_ACTIVITY_WINDOW_MS: u64 = crate::activity::PREVIEW_ACTIVITY_WINDOW_MS;
/// Default preview-activity scrape cadence. Must stay below the withhold window.
const DEFAULT_PREVIEW_ACTIVITY_SCRAPE_INTERVAL_MS: u64 = 10_000;
/// Smallest window that still leaves room for a strictly-smaller scrape; only a
/// broken config reaches it (the normal window is 60s).
const MIN_PREVIEW_ACTIVITY_WINDOW_MS: u64 = 2;
/// Scrape-interval floor; `0` would busy-loop the scraper.
const MIN_PREVIEW_ACTIVITY_SCRAPE_INTERVAL_MS: u64 = 1;

/// Tunable timing/threshold constants for the workspace tool server.
#[derive(Debug, Clone)]
pub struct StatusConfig {
    /// Interval between status/heartbeat emissions.
    pub heartbeat: Duration,
    /// Transport keepalive interval. Should exceed `heartbeat`.
    pub keepalive: Duration,
    /// WebSocket keepalive ping cadence for the server SDK connection.
    pub ws_ping: Duration,
    /// Reconnect backoff schedule for the server SDK connection. `None` leaves
    /// the SDK's built-in default exponential schedule in place.
    pub ws_reconnect_backoff: Option<Vec<Duration>>,
    /// Number of consecutive server reconnect failures before warning.
    pub hub_warn_threshold: u32,
    /// Base delay for exponential backoff on failed server event-notification sends.
    pub hub_backoff_base: Duration,
    /// Idle duration after which an inactive session is pruned.
    pub session_idle_prune: Duration,
    /// Legacy single-phase drain timeout (`GROK_WORKSPACE_DRAIN_TIMEOUT_SECS`),
    /// retained for compatibility; the SIGTERM and server-evict paths now use the
    /// two-phase drain bounded by `GROK_WORKSPACE_TERMINATION_GRACE_MS`.
    pub drain_timeout: Duration,
    /// Per-call timeout for agent RPCs.
    pub agent_rpc_timeout: Duration,
    /// Timeout for establishing an agent connection.
    pub agent_connect_timeout: Duration,
    /// Opt-in foreground-only idle (`GROK_WORKSPACE_IDLE_IGNORE_BACKGROUND_TASKS`);
    /// requires the literal `"true"` — other spellings fall back to this default.
    pub idle_ignores_background: bool,
    /// Recent preview-proxy traffic withholds idle for this window
    /// (`GROK_WORKSPACE_PREVIEW_ACTIVITY_WINDOW_MS`).
    pub preview_activity_window: Duration,
    /// Cadence at which the preview-activity scraper polls the proxy
    /// (`GROK_WORKSPACE_PREVIEW_ACTIVITY_SCRAPE_INTERVAL_MS`); kept strictly
    /// below `preview_activity_window` by [`validate`](Self::validate).
    pub preview_activity_scrape_interval: Duration,
    /// True when this container booted via the sandbox restore path, which
    /// injects `GROK_SESSION_RESTORED=true`; a first boot never does.
    pub session_restored: bool,
    /// True when restore injects `GROK_REVIVE_SCRIPT_CONFIGURED=true` (launchable
    /// revive configured); unset on first boot and non-launchable restores.
    pub revive_script_configured: bool,
}

impl Default for StatusConfig {
    fn default() -> Self {
        Self {
            heartbeat: Duration::from_secs(DEFAULT_HEARTBEAT_SECS),
            keepalive: Duration::from_secs(DEFAULT_KEEPALIVE_SECS),
            ws_ping: Duration::from_secs(DEFAULT_WS_PING_SECS),
            ws_reconnect_backoff: None,
            hub_warn_threshold: DEFAULT_HUB_WARN_THRESHOLD,
            hub_backoff_base: Duration::from_millis(DEFAULT_HUB_BACKOFF_BASE_MS),
            session_idle_prune: Duration::from_secs(DEFAULT_SESSION_IDLE_PRUNE_SECS),
            drain_timeout: Duration::from_secs(DEFAULT_DRAIN_TIMEOUT_SECS),
            agent_rpc_timeout: Duration::from_secs(DEFAULT_AGENT_RPC_TIMEOUT_SECS),
            agent_connect_timeout: Duration::from_secs(DEFAULT_AGENT_CONNECT_TIMEOUT_SECS),
            idle_ignores_background: false,
            preview_activity_window: Duration::from_millis(DEFAULT_PREVIEW_ACTIVITY_WINDOW_MS),
            preview_activity_scrape_interval: Duration::from_millis(
                DEFAULT_PREVIEW_ACTIVITY_SCRAPE_INTERVAL_MS,
            ),
            session_restored: false,
            revive_script_configured: false,
        }
    }
}

impl StatusConfig {
    /// Populate from `GROK_WORKSPACE_*`. Unset or unparseable vars fall
    /// back to the default with a `warn!`. Never fails.
    pub fn from_env() -> Self {
        let defaults = Self::default();
        let (agent_rpc, agent_connect) = Self::agent_timeouts_from_env();
        let mut cfg = Self {
            heartbeat: secs_or("GROK_WORKSPACE_HEARTBEAT_SECS", defaults.heartbeat),
            keepalive: secs_or("GROK_WORKSPACE_KEEPALIVE_SECS", defaults.keepalive),
            ws_ping: secs_or("GROK_WORKSPACE_WS_PING_SECS", defaults.ws_ping),
            ws_reconnect_backoff: backoff_schedule_from_env(
                "GROK_WORKSPACE_WS_RECONNECT_BACKOFF_MS",
            ),
            hub_warn_threshold: parse_or(
                "GROK_WORKSPACE_HUB_WARN_THRESHOLD",
                defaults.hub_warn_threshold,
            ),
            hub_backoff_base: ms_or(
                "GROK_WORKSPACE_HUB_BACKOFF_BASE_MS",
                defaults.hub_backoff_base,
            ),
            session_idle_prune: secs_or(
                "GROK_WORKSPACE_SESSION_IDLE_PRUNE_SECS",
                defaults.session_idle_prune,
            ),
            drain_timeout: secs_or("GROK_WORKSPACE_DRAIN_TIMEOUT_SECS", defaults.drain_timeout),
            agent_rpc_timeout: agent_rpc,
            agent_connect_timeout: agent_connect,
            idle_ignores_background: parse_or(
                "GROK_WORKSPACE_IDLE_IGNORE_BACKGROUND_TASKS",
                defaults.idle_ignores_background,
            ),
            preview_activity_window: ms_or(
                "GROK_WORKSPACE_PREVIEW_ACTIVITY_WINDOW_MS",
                defaults.preview_activity_window,
            ),
            preview_activity_scrape_interval: ms_or(
                "GROK_WORKSPACE_PREVIEW_ACTIVITY_SCRAPE_INTERVAL_MS",
                defaults.preview_activity_scrape_interval,
            ),
            session_restored: std::env::var("GROK_SESSION_RESTORED").as_deref() == Ok("true"),
            revive_script_configured: std::env::var("GROK_REVIVE_SCRIPT_CONFIGURED").as_deref()
                == Ok("true"),
        };
        cfg.validate();
        cfg
    }

    /// Read only the agent gRPC `(request, connect)` timeouts from the
    /// environment, without parsing or validating the rest of the config.
    ///
    /// Used by the btrfs delegate's env-based construction path, which has no
    /// `StatusConfig` in scope; reading just these two vars avoids re-running
    /// [`validate`](Self::validate) (and its possible duplicate `warn!`).
    pub fn agent_timeouts_from_env() -> (Duration, Duration) {
        let defaults = Self::default();
        const RPC_VAR: &str = "GROK_WORKSPACE_AGENT_RPC_TIMEOUT_SECS";
        const CONNECT_VAR: &str = "GROK_WORKSPACE_AGENT_CONNECT_TIMEOUT_SECS";
        (
            nonzero_secs_or(
                RPC_VAR,
                parse_or(RPC_VAR, defaults.agent_rpc_timeout.as_secs()),
                defaults.agent_rpc_timeout,
            ),
            nonzero_secs_or(
                CONNECT_VAR,
                parse_or(CONNECT_VAR, defaults.agent_connect_timeout.as_secs()),
                defaults.agent_connect_timeout,
            ),
        )
    }

    /// Warn on (and, where load-bearing, repair) inconsistent values.
    ///
    /// `keepalive` can't be validated against the server's idle window (unknown
    /// here), so it only warns. The preview scraper, however, must run strictly
    /// more often than the withhold window (else the withhold lapses between
    /// scrapes) and never at a zero interval (which would busy-loop it), so any
    /// misconfiguration is repaired into `1ms <= scrape < window`.
    fn validate(&mut self) {
        if self.keepalive <= self.heartbeat {
            tracing::warn!(
                keepalive = ?self.keepalive,
                heartbeat = ?self.heartbeat,
                "GROK_WORKSPACE keepalive <= heartbeat; transport may time out between heartbeats"
            );
        }
        let min_scrape = Duration::from_millis(MIN_PREVIEW_ACTIVITY_SCRAPE_INTERVAL_MS);
        let window = self
            .preview_activity_window
            .max(Duration::from_millis(MIN_PREVIEW_ACTIVITY_WINDOW_MS));
        let scrape = self
            .preview_activity_scrape_interval
            .clamp(min_scrape, window - min_scrape);
        if window != self.preview_activity_window || scrape != self.preview_activity_scrape_interval
        {
            tracing::warn!(
                scrape_interval = ?self.preview_activity_scrape_interval,
                window = ?self.preview_activity_window,
                clamped_scrape = ?scrape,
                clamped_window = ?window,
                "GROK_WORKSPACE preview scrape interval/window out of range; clamped to 1ms <= scrape < window"
            );
            self.preview_activity_window = window;
            self.preview_activity_scrape_interval = scrape;
        }
    }
}

/// Read `var` and parse it as `T`. Returns `default` when unset; warns and
/// returns `default` when present but unparseable.
fn parse_or<T: FromStr>(var: &str, default: T) -> T {
    match std::env::var(var) {
        Err(_) => default,
        Ok(raw) => match raw.parse::<T>() {
            Ok(value) => value,
            Err(_) => {
                tracing::warn!(var, value = %raw, "Unparseable GROK_WORKSPACE value; using default");
                default
            }
        },
    }
}

/// Parse `var` as a `u64` number of seconds into a [`Duration`].
fn secs_or(var: &str, default: Duration) -> Duration {
    Duration::from_secs(parse_or(var, default.as_secs()))
}

/// Parse `var` as a `u64` number of milliseconds into a [`Duration`].
fn ms_or(var: &str, default: Duration) -> Duration {
    Duration::from_millis(parse_or(var, default.as_millis() as u64))
}

/// Convert a parsed seconds value into a [`Duration`], rejecting `0`. A zero
/// gRPC timeout makes every RPC fail immediately, so a configured `0` warns
/// and falls back to `default` instead of being applied verbatim.
fn nonzero_secs_or(var: &str, secs: u64, default: Duration) -> Duration {
    if secs == 0 {
        tracing::warn!(
            var,
            default = ?default,
            "GROK_WORKSPACE agent timeout of 0s is invalid; using default"
        );
        return default;
    }
    Duration::from_secs(secs)
}

/// Parse `var` as a comma-separated list of `u64` milliseconds into a reconnect
/// backoff schedule. Returns `None` (keep the SDK's built-in default schedule)
/// when the var is unset or yields no values, and warns + returns `None` when
/// any element fails to parse.
fn backoff_schedule_from_env(var: &str) -> Option<Vec<Duration>> {
    let raw = std::env::var(var).ok()?;
    let mut schedule = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.parse::<u64>() {
            Ok(ms) => schedule.push(Duration::from_millis(ms)),
            Err(_) => {
                tracing::warn!(
                    var,
                    value = %raw,
                    "Unparseable GROK_WORKSPACE backoff schedule; using SDK default"
                );
                return None;
            }
        }
    }
    (!schedule.is_empty()).then_some(schedule)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Crate-shared env lock: every test that mutates the process environment
    // holds it for its full duration. ONE lock for the whole crate because the
    // hazard is the global `environ` array under `unsafe set_var` (not the
    // variable's value), so even disjoint vars must serialize.
    use crate::ENV_TEST_LOCK as ENV_LOCK;

    #[test]
    fn defaults_match_documented_values() {
        let cfg = StatusConfig::default();
        assert_eq!(cfg.heartbeat, Duration::from_secs(30));
        assert_eq!(cfg.keepalive, Duration::from_secs(60));
        assert_eq!(cfg.ws_ping, Duration::from_secs(30));
        assert_eq!(cfg.ws_reconnect_backoff, None);
        assert_eq!(cfg.hub_warn_threshold, 5);
        assert_eq!(cfg.hub_backoff_base, Duration::from_millis(100));
        assert_eq!(cfg.session_idle_prune, Duration::from_secs(1800));
        assert_eq!(cfg.drain_timeout, Duration::from_secs(30));
        assert_eq!(cfg.agent_rpc_timeout, Duration::from_secs(30));
        assert_eq!(cfg.agent_connect_timeout, Duration::from_secs(5));
        assert!(!cfg.idle_ignores_background);
        assert_eq!(cfg.preview_activity_window, Duration::from_secs(60));
        assert_eq!(
            cfg.preview_activity_scrape_interval,
            Duration::from_secs(10)
        );
        assert!(!cfg.session_restored);
        assert!(!cfg.revive_script_configured);
    }

    /// `parse_or` returns the default when the variable is unset. Uses a
    /// uniquely-named var so it never collides with other tests' env writes.
    #[test]
    fn parse_or_unset_returns_default() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let var = "GROK_WORKSPACE_TEST_PARSE_OR_UNSET";
        unsafe { std::env::remove_var(var) };
        assert_eq!(parse_or::<u32>(var, 5), 5);
    }

    #[test]
    fn parse_or_valid_parses() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let var = "GROK_WORKSPACE_TEST_PARSE_OR_VALID";
        unsafe { std::env::set_var(var, "42") };
        assert_eq!(parse_or::<u32>(var, 5), 42);
        unsafe { std::env::remove_var(var) };
    }

    #[test]
    fn parse_or_invalid_falls_back_without_panic() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let var = "GROK_WORKSPACE_TEST_PARSE_OR_INVALID";
        unsafe { std::env::set_var(var, "not-a-number") };
        assert_eq!(parse_or::<u32>(var, 5), 5);
        unsafe { std::env::remove_var(var) };
    }

    #[test]
    fn secs_or_parses_into_duration() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let var = "GROK_WORKSPACE_TEST_SECS_OR_VALID";
        unsafe { std::env::set_var(var, "120") };
        assert_eq!(
            secs_or(var, Duration::from_secs(30)),
            Duration::from_secs(120)
        );
        unsafe { std::env::remove_var(var) };
    }

    #[test]
    fn secs_or_unset_returns_default() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let var = "GROK_WORKSPACE_TEST_SECS_OR_UNSET";
        unsafe { std::env::remove_var(var) };
        assert_eq!(
            secs_or(var, Duration::from_secs(30)),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn secs_or_invalid_falls_back_without_panic() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let var = "GROK_WORKSPACE_TEST_SECS_OR_INVALID";
        unsafe { std::env::set_var(var, "12.5") };
        assert_eq!(
            secs_or(var, Duration::from_secs(30)),
            Duration::from_secs(30)
        );
        unsafe { std::env::remove_var(var) };
    }

    #[test]
    fn ms_or_parses_into_duration() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let var = "GROK_WORKSPACE_TEST_MS_OR_VALID";
        unsafe { std::env::set_var(var, "250") };
        assert_eq!(
            ms_or(var, Duration::from_millis(100)),
            Duration::from_millis(250)
        );
        unsafe { std::env::remove_var(var) };
    }

    #[test]
    fn ms_or_invalid_falls_back_without_panic() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let var = "GROK_WORKSPACE_TEST_MS_OR_INVALID";
        unsafe { std::env::set_var(var, "abc") };
        assert_eq!(
            ms_or(var, Duration::from_millis(100)),
            Duration::from_millis(100)
        );
        unsafe { std::env::remove_var(var) };
    }

    /// With none of the `GROK_WORKSPACE_*` vars set, `from_env` reproduces
    /// `StatusConfig::default()` field-for-field.
    ///
    /// This is the one test that touches the real (non-`_TEST_`-prefixed)
    /// var names: it `remove_var`s all of them before reading them. If a future
    /// test sets these shared names, run them serialized to avoid a race.
    #[test]
    fn from_env_clean_matches_default() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for var in [
            "GROK_WORKSPACE_HEARTBEAT_SECS",
            "GROK_WORKSPACE_KEEPALIVE_SECS",
            "GROK_WORKSPACE_WS_PING_SECS",
            "GROK_WORKSPACE_WS_RECONNECT_BACKOFF_MS",
            "GROK_WORKSPACE_HUB_WARN_THRESHOLD",
            "GROK_WORKSPACE_HUB_BACKOFF_BASE_MS",
            "GROK_WORKSPACE_SESSION_IDLE_PRUNE_SECS",
            "GROK_WORKSPACE_DRAIN_TIMEOUT_SECS",
            "GROK_WORKSPACE_AGENT_RPC_TIMEOUT_SECS",
            "GROK_WORKSPACE_AGENT_CONNECT_TIMEOUT_SECS",
            "GROK_WORKSPACE_IDLE_IGNORE_BACKGROUND_TASKS",
            "GROK_WORKSPACE_PREVIEW_ACTIVITY_WINDOW_MS",
            "GROK_WORKSPACE_PREVIEW_ACTIVITY_SCRAPE_INTERVAL_MS",
            "GROK_SESSION_RESTORED",
            "GROK_REVIVE_SCRIPT_CONFIGURED",
        ] {
            unsafe { std::env::remove_var(var) };
        }
        let cfg = StatusConfig::from_env();
        let default = StatusConfig::default();
        assert_eq!(cfg.heartbeat, default.heartbeat);
        assert_eq!(cfg.keepalive, default.keepalive);
        assert_eq!(cfg.ws_ping, default.ws_ping);
        assert_eq!(cfg.ws_reconnect_backoff, default.ws_reconnect_backoff);
        assert_eq!(cfg.hub_warn_threshold, default.hub_warn_threshold);
        assert_eq!(cfg.hub_backoff_base, default.hub_backoff_base);
        assert_eq!(cfg.session_idle_prune, default.session_idle_prune);
        assert_eq!(cfg.drain_timeout, default.drain_timeout);
        assert_eq!(cfg.agent_rpc_timeout, default.agent_rpc_timeout);
        assert_eq!(cfg.agent_connect_timeout, default.agent_connect_timeout);
        assert_eq!(cfg.idle_ignores_background, default.idle_ignores_background);
        assert_eq!(cfg.preview_activity_window, default.preview_activity_window);
        assert_eq!(
            cfg.preview_activity_scrape_interval,
            default.preview_activity_scrape_interval
        );
        assert_eq!(cfg.session_restored, default.session_restored);
        assert_eq!(
            cfg.revive_script_configured,
            default.revive_script_configured
        );
    }

    #[test]
    fn from_env_reads_session_restored_true_only() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("GROK_SESSION_RESTORED", "true") };
        let restored = StatusConfig::from_env().session_restored;
        unsafe { std::env::set_var("GROK_SESSION_RESTORED", "1") };
        let non_canonical = StatusConfig::from_env().session_restored;
        unsafe { std::env::remove_var("GROK_SESSION_RESTORED") };
        assert!(restored);
        assert!(!non_canonical);
    }

    #[test]
    fn from_env_reads_revive_script_configured_true_only() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("GROK_REVIVE_SCRIPT_CONFIGURED", "true") };
        let configured = StatusConfig::from_env().revive_script_configured;
        unsafe { std::env::set_var("GROK_REVIVE_SCRIPT_CONFIGURED", "1") };
        let non_canonical = StatusConfig::from_env().revive_script_configured;
        unsafe { std::env::remove_var("GROK_REVIVE_SCRIPT_CONFIGURED") };
        assert!(configured);
        assert!(!non_canonical);
    }

    #[test]
    fn from_env_reads_idle_ignore_background_true() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("GROK_WORKSPACE_IDLE_IGNORE_BACKGROUND_TASKS", "true") };
        let cfg = StatusConfig::from_env();
        unsafe { std::env::remove_var("GROK_WORKSPACE_IDLE_IGNORE_BACKGROUND_TASKS") };
        assert!(cfg.idle_ignores_background);
    }

    #[test]
    fn from_env_reads_preview_activity_window() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("GROK_WORKSPACE_PREVIEW_ACTIVITY_WINDOW_MS", "120000") };
        let cfg = StatusConfig::from_env();
        unsafe { std::env::remove_var("GROK_WORKSPACE_PREVIEW_ACTIVITY_WINDOW_MS") };
        assert_eq!(cfg.preview_activity_window, Duration::from_millis(120_000));
    }

    #[test]
    fn from_env_reads_preview_activity_scrape_interval() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("GROK_WORKSPACE_PREVIEW_ACTIVITY_SCRAPE_INTERVAL_MS", "5000") };
        let cfg = StatusConfig::from_env();
        unsafe { std::env::remove_var("GROK_WORKSPACE_PREVIEW_ACTIVITY_SCRAPE_INTERVAL_MS") };
        assert_eq!(
            cfg.preview_activity_scrape_interval,
            Duration::from_millis(5_000)
        );
    }

    /// `validate` must not panic regardless of the relative ordering of
    /// `keepalive` and `heartbeat`.
    #[test]
    fn validate_does_not_panic_when_keepalive_le_heartbeat() {
        let mut cfg = StatusConfig {
            keepalive: Duration::from_secs(10),
            heartbeat: Duration::from_secs(30),
            ..StatusConfig::default()
        };
        cfg.validate();
    }

    #[test]
    fn validate_clamps_preview_scrape_into_valid_range() {
        for (window_ms, scrape_ms, exp_window_ms, exp_scrape_ms) in [
            (1_000u64, 4_000u64, 1_000u64, 999u64),
            (1_000, 1_000, 1_000, 999),
            (1_000, 200, 1_000, 200),
            (60_000, 0, 60_000, 1),
            (0, 0, 2, 1),
            (1, 5, 2, 1),
        ] {
            let mut cfg = StatusConfig {
                preview_activity_window: Duration::from_millis(window_ms),
                preview_activity_scrape_interval: Duration::from_millis(scrape_ms),
                ..StatusConfig::default()
            };
            cfg.validate();
            assert_eq!(
                cfg.preview_activity_window,
                Duration::from_millis(exp_window_ms)
            );
            assert_eq!(
                cfg.preview_activity_scrape_interval,
                Duration::from_millis(exp_scrape_ms)
            );
            assert!(cfg.preview_activity_scrape_interval >= Duration::from_millis(1));
            assert!(cfg.preview_activity_scrape_interval < cfg.preview_activity_window);
        }
    }

    /// A configured agent timeout of `0` seconds is invalid (it would make
    /// every gRPC call fail immediately), so `nonzero_secs_or` falls back to
    /// the supplied default instead of returning `Duration::ZERO`.
    #[test]
    fn nonzero_secs_or_zero_falls_back_to_default() {
        assert_eq!(
            nonzero_secs_or(
                "GROK_WORKSPACE_AGENT_RPC_TIMEOUT_SECS",
                0,
                Duration::from_secs(30)
            ),
            Duration::from_secs(30)
        );
        assert_eq!(
            nonzero_secs_or(
                "GROK_WORKSPACE_AGENT_CONNECT_TIMEOUT_SECS",
                0,
                Duration::from_secs(5)
            ),
            Duration::from_secs(5)
        );
    }

    /// A positive value is honored verbatim.
    #[test]
    fn nonzero_secs_or_positive_is_passed_through() {
        assert_eq!(
            nonzero_secs_or(
                "GROK_WORKSPACE_AGENT_RPC_TIMEOUT_SECS",
                12,
                Duration::from_secs(30)
            ),
            Duration::from_secs(12)
        );
    }

    /// An unset backoff var leaves the schedule unconfigured (`None`), so the
    /// SDK keeps its built-in default.
    #[test]
    fn backoff_schedule_unset_returns_none() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let var = "GROK_WORKSPACE_TEST_BACKOFF_UNSET";
        unsafe { std::env::remove_var(var) };
        assert_eq!(backoff_schedule_from_env(var), None);
    }

    /// A valid comma-separated list parses into millisecond `Duration`s in
    /// order, tolerating surrounding whitespace.
    #[test]
    fn backoff_schedule_valid_list_parses_in_order() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let var = "GROK_WORKSPACE_TEST_BACKOFF_VALID";
        unsafe { std::env::set_var(var, "100, 200,500,1000") };
        assert_eq!(
            backoff_schedule_from_env(var),
            Some(vec![
                Duration::from_millis(100),
                Duration::from_millis(200),
                Duration::from_millis(500),
                Duration::from_millis(1000),
            ])
        );
        unsafe { std::env::remove_var(var) };
    }

    /// A malformed element makes the whole schedule fall back to `None` (and
    /// warns) rather than silently dropping entries.
    #[test]
    fn backoff_schedule_malformed_returns_none() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let var = "GROK_WORKSPACE_TEST_BACKOFF_MALFORMED";
        unsafe { std::env::set_var(var, "100,not-a-number,500") };
        assert_eq!(backoff_schedule_from_env(var), None);
        unsafe { std::env::remove_var(var) };
    }
}
