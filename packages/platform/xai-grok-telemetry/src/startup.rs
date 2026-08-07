//! Named startup phases on a per-process timer, reported once to
//! `unified.jsonl`, product events, and OTLP metrics. A closed schema with
//! pinned metric keys: time anything else with a `tracing` span, or give it
//! its own schema.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// `unified.jsonl` message keys, exported so consumers (the probe, tests)
/// grep for the same strings this module writes.
pub const STARTUP_PHASE_MSG: &str = "startup phase";
pub const CONNECT_FINISHED_MSG: &str = "connect finished";
pub const STARTUP_COMPLETE_MSG: &str = "startup complete";
pub const STARTUP_TIMING_MSG: &str = "startup timing";

#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum StartupPhase {
    LoadConfig,
    ManagedPolicy,
    Bootstrap,
    ModelCatalog,
    SpawnWorker,
    LeaderConnect,
    AcpInitialize,
    EagerAuth,
    AppInit,
    SessionCreate,
}

impl StartupPhase {
    pub fn label(self) -> &'static str {
        self.into()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::IntoStaticStr, serde::Serialize)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum StartupOutcome {
    Ok,
    Timeout,
    Cancelled,
    Error,
}

impl StartupOutcome {
    pub fn label(self) -> &'static str {
        self.into()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, strum::IntoStaticStr, serde::Serialize)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    #[default]
    Unknown,
    Personal,
    Team,
    Deployment,
}

impl AuthMode {
    pub fn label(self) -> &'static str {
        self.into()
    }
}

/// Who reports the timer, so an embedded run does not report it twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Client,
    Agent,
}

/// The kind of agent process a connect attempt targets. `label` is the
/// telemetry token; `Display` is the prose in user-facing errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::IntoStaticStr, serde::Serialize)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    Embedded,
    Leader,
}

impl AgentKind {
    pub fn label(self) -> &'static str {
        self.into()
    }
}

impl std::fmt::Display for AgentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Embedded => "the embedded agent",
            Self::Leader => "the grok leader",
        })
    }
}

struct Inner {
    completed: Vec<(StartupPhase, Duration)>,
    current: Option<(StartupPhase, Instant)>,
    auth_mode: AuthMode,
    owner: Owner,
}

pub struct StartupTimer {
    started: Instant,
    inner: Mutex<Inner>,
}

impl StartupTimer {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            inner: Mutex::new(Inner {
                completed: Vec::new(),
                current: None,
                auth_mode: AuthMode::Unknown,
                owner: Owner::Agent,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Closes the open phase; re-entering the open phase is ignored, so two
    /// layers can name the same step and it is measured once.
    pub fn enter(&self, phase: StartupPhase) {
        let now = Instant::now();
        {
            let mut g = self.lock();
            if matches!(g.current, Some((open, _)) if open == phase) {
                return;
            }
            if let Some((prev, t0)) = g.current.take() {
                g.completed.push((prev, now.saturating_duration_since(t0)));
            }
            g.current = Some((phase, now));
        }
        let elapsed_ms = self.started.elapsed().as_millis() as u64;
        tracing::info!(phase = %phase.label(), elapsed_ms, "startup phase");
        crate::unified_log::info(
            STARTUP_PHASE_MSG,
            None,
            Some(serde_json::json!({ "phase": phase.label(), "elapsed_ms": elapsed_ms })),
        );
    }

    fn close_open_phase(&self) {
        let now = Instant::now();
        let mut g = self.lock();
        if let Some((prev, t0)) = g.current.take() {
            g.completed.push((prev, now.saturating_duration_since(t0)));
        }
    }

    pub fn set_auth_mode(&self, mode: AuthMode) {
        self.lock().auth_mode = mode;
    }

    pub fn auth_mode(&self) -> AuthMode {
        self.lock().auth_mode
    }

    pub fn owner(&self) -> Owner {
        self.lock().owner
    }

    pub fn stuck_in(&self) -> &'static str {
        self.lock()
            .current
            .map(|(p, _)| p.label())
            .unwrap_or("unknown")
    }

    /// Completed phases read `phase=dur`; the open one reads `phase>=dur`.
    pub fn summary(&self) -> String {
        let now = Instant::now();
        let g = self.lock();
        if g.completed.is_empty() && g.current.is_none() {
            return "no phases entered".to_string();
        }
        let mut out = String::new();
        for (phase, d) in &g.completed {
            if !out.is_empty() {
                out.push_str(", ");
            }
            let _ = write!(out, "{}={}", phase.label(), format_duration(*d));
        }
        if let Some((phase, t0)) = g.current {
            if !out.is_empty() {
                out.push_str(", ");
            }
            let open = now.saturating_duration_since(t0);
            let _ = write!(out, "{}>={}", phase.label(), format_duration(open));
        }
        out
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn phase_durations_ms(&self) -> BTreeMap<String, u64> {
        let now = Instant::now();
        let g = self.lock();
        let mut map: BTreeMap<String, u64> = BTreeMap::new();
        for (phase, d) in &g.completed {
            *map.entry(phase.label().to_string()).or_default() += d.as_millis() as u64;
        }
        if let Some((phase, t0)) = g.current {
            *map.entry(phase.label().to_string()).or_default() +=
                now.saturating_duration_since(t0).as_millis() as u64;
        }
        map
    }

    pub fn emit_telemetry(
        &self,
        connect_target: AgentKind,
        outcome: StartupOutcome,
        timeout_secs: Option<u64>,
        embedded_fallback: bool,
    ) {
        // A finished attempt has no open phase; later work is not connect time.
        if outcome == StartupOutcome::Ok {
            self.close_open_phase();
        }
        let stuck_in = (outcome == StartupOutcome::Timeout).then(|| self.stuck_in().to_string());
        let phases = self.summary();
        let elapsed_ms = self.elapsed().as_millis() as u64;
        crate::unified_log::info(
            CONNECT_FINISHED_MSG,
            None,
            Some(serde_json::json!({
                "connect_target": connect_target,
                "outcome": outcome,
                "stuck_in": stuck_in,
                "phases": phases,
                "elapsed_ms": elapsed_ms,
                "auth_mode": self.auth_mode(),
            })),
        );
        crate::session_ctx::log_event(crate::events::AgentConnect {
            connect_target,
            outcome,
            stuck_in,
            phases,
            phase_durations_ms: self.phase_durations_ms(),
            elapsed_ms,
            timeout_secs,
            embedded_fallback,
            auth_mode: self.auth_mode(),
        });
    }
}

impl Default for StartupTimer {
    fn default() -> Self {
        Self::new()
    }
}

static CURRENT: Mutex<Option<Arc<StartupTimer>>> = Mutex::new(None);
static DONE: AtomicBool = AtomicBool::new(false);
static PROCESS_START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Call first in `main`; the clock otherwise starts at first use and
/// totals undercount.
pub fn mark_process_start() {
    LazyLock::force(&PROCESS_START);
}

pub fn process_elapsed() -> Duration {
    PROCESS_START.elapsed()
}

fn current() -> Option<Arc<StartupTimer>> {
    CURRENT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(Arc::clone)
}

/// Install a new attempt, unless the latch is already set: once
/// [`report_total`] or [`clear`] ran, this process's startup is over and the
/// returned timer records locally only.
pub fn begin(owner: Owner) -> Arc<StartupTimer> {
    let timer = Arc::new(StartupTimer::new());
    timer.lock().owner = owner;
    let mut current = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    if !DONE.load(Ordering::Relaxed) {
        *current = Some(Arc::clone(&timer));
    }
    timer
}

pub fn agent_owned() -> Option<Arc<StartupTimer>> {
    current().filter(|p| p.owner() == Owner::Agent)
}

pub(crate) fn is_active() -> bool {
    !DONE.load(Ordering::Relaxed) && current().is_some()
}

pub fn clear() {
    DONE.store(true, Ordering::Relaxed);
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Lazily installs an agent-owned timer, covering the standalone leader
/// and agent server; a no-op once startup is done.
pub fn enter(phase: StartupPhase) {
    if DONE.load(Ordering::Relaxed) {
        return;
    }
    let timer = match current() {
        Some(timer) => timer,
        None => begin(Owner::Agent),
    };
    timer.enter(phase);
}

/// Scopes a phase to a region of work: entered on creation, closed on drop,
/// so no failure return can leave the phase open across a retry wait.
#[must_use = "the phase closes when this guard drops"]
pub struct PhaseScope(());

impl Drop for PhaseScope {
    fn drop(&mut self) {
        if let Some(timer) = current() {
            timer.close_open_phase();
        }
    }
}

/// Enter `phase` for the lifetime of the returned guard.
pub fn phase_scope(phase: StartupPhase) -> PhaseScope {
    enter(phase);
    PhaseScope(())
}

pub fn set_auth_mode(mode: AuthMode) {
    if let Some(timer) = current() {
        timer.set_auth_mode(mode);
    }
}

/// Reports the startup total and latches done, at most once per process.
/// `Ok` means the first usable session; failure outcomes are for terminal
/// startup failures only. A transient failure (a session create the user can
/// retry) reports nothing, so the eventual success still records.
pub fn report_total(outcome: StartupOutcome) {
    if DONE.swap(true, Ordering::Relaxed) {
        return;
    }
    let timer = CURRENT.lock().unwrap_or_else(|e| e.into_inner()).take();
    let total_ms = process_elapsed().as_millis() as u64;
    let (phases, auth_mode) = match timer {
        Some(p) => {
            if outcome == StartupOutcome::Ok {
                p.close_open_phase();
            }
            (p.summary(), p.auth_mode())
        }
        None => (String::new(), AuthMode::Unknown),
    };
    crate::unified_log::info(
        STARTUP_COMPLETE_MSG,
        None,
        Some(serde_json::json!({
            "total_ms": total_ms,
            "outcome": outcome,
            "phases": phases,
            "auth_mode": auth_mode,
        })),
    );
    crate::session_ctx::log_event(crate::events::StartupComplete {
        total_ms,
        outcome,
        phases,
        auth_mode,
    });
}

fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_tracks_completed_and_open_phases() {
        let p = StartupTimer::new();
        p.enter(StartupPhase::LoadConfig);
        p.enter(StartupPhase::ManagedPolicy);
        p.enter(StartupPhase::ModelCatalog);

        let s = p.summary();
        assert!(s.contains("load_config="), "{s}");
        assert!(s.contains("managed_policy="), "{s}");
        assert!(s.contains("model_catalog>="), "{s}");
        assert_eq!(p.stuck_in(), "model_catalog");
        let d = p.phase_durations_ms();
        assert!(
            d.contains_key("load_config") && d.contains_key("model_catalog"),
            "{d:?}"
        );
    }

    /// One test for the whole lifecycle: the statics are process-wide, so
    /// interleaved tests would race each other.
    #[test]
    fn global_lifecycle_records_then_latches_done() {
        crate::unified_log::redirect_to_temp_for_tests();

        let p = begin(Owner::Client);
        enter(StartupPhase::ManagedPolicy);
        set_auth_mode(AuthMode::Deployment);
        assert_eq!(p.stuck_in(), "managed_policy");
        assert_eq!(p.auth_mode().label(), "deployment");
        assert!(
            agent_owned().is_none(),
            "client-owned: agent must not report"
        );

        // A fallback attempt before any latch replaces the install; the old
        // handle keeps its own history.
        let p2 = begin(Owner::Client);
        enter(StartupPhase::Bootstrap);
        assert_eq!(p2.stuck_in(), "bootstrap");
        assert_eq!(p.stuck_in(), "managed_policy");

        drop(crate::instrumentation::timer("startup.mirror_probe_active"));

        enter(StartupPhase::SessionCreate);
        report_total(StartupOutcome::Ok);

        drop(crate::instrumentation::timer("startup.mirror_probe_done"));
        let log = String::from_utf8_lossy(&crate::unified_log::snapshot_log().unwrap_or_default())
            .into_owned();
        assert!(log.contains("startup.mirror_probe_active"), "{log}");
        assert!(
            !log.contains("startup.mirror_probe_done"),
            "done: timers must not mirror, {log}"
        );

        // Latched: no re-report, no recording, and `begin` cannot re-arm.
        report_total(StartupOutcome::Ok);
        enter(StartupPhase::ModelCatalog);
        assert_eq!(p2.stuck_in(), "unknown", "ok total closes the open phase");
        assert!(p2.summary().contains("session_create="), "{}", p2.summary());
        let p3 = begin(Owner::Agent);
        enter(StartupPhase::LoadConfig);
        assert_eq!(p3.stuck_in(), "unknown", "latched: enter records nothing");

        clear();
        assert!(agent_owned().is_none(), "cleared: nothing installed");
    }
}
