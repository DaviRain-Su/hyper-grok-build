//! One shared startup fetch of `/v1/models` and `/v1/settings` per process.
//! A process global because the begin side (pager) and the commit side
//! (`bootstrap`) share no owner object. The worker never writes: the models
//! cache write lands only in [`accept`], after the policy re-checks.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::{
    ModelsCacheWrite, SettingsCacheWrite, prefetch_env, resolve_disk_auth,
    resolve_startup_endpoints,
};
use crate::agent::config::Config;
use crate::auth::{GrokAuth, GrokComConfig};
use crate::util::config::RemoteSettings;

static INFLIGHT: Mutex<Option<Arc<Inflight>>> = Mutex::new(None);

/// Mirrors the worker's own HTTP budget, with margin for backoff sleeps.
const ACCEPT_DEADLINE: Duration = Duration::from_millis(
    crate::http::STARTUP_FETCH_TIMEOUT.as_millis() as u64
        * (2 + crate::http::SETTINGS_FETCH_MAX_ATTEMPTS as u64)
        + 5_000,
);

struct Inflight {
    origin: String,
    state: Mutex<State>,
    done: Condvar,
}

#[derive(Default)]
struct State {
    finished: bool,
    panicked: bool,
    settings: Option<RemoteSettings>,
    models_write: Option<ModelsCacheWrite>,
    settings_write: Option<SettingsCacheWrite>,
}

/// Marks the fetch finished even if the worker panics, so waiters never hang.
struct FinishGuard(Arc<Inflight>);

impl Drop for FinishGuard {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.finished = true;
        state.panicked = std::thread::panicking();
        drop(state);
        self.0.done.notify_all();
    }
}

/// Pre-gate start: no managed-config sync, and nothing starts while a policy
/// repair is pending (no authenticated request under an untrusted policy).
pub fn begin_before_policy_gate(cfg: &Config) -> bool {
    if cfg!(test) {
        return INFLIGHT.lock().unwrap().is_some();
    }
    if cfg.remote_settings.is_some() || crate::managed_config::policy_repair_pending() {
        return false;
    }
    begin_inner(|| resolve_disk_auth(Some(cfg.grok_com_config.clone())))
}

/// Not repair-guarded (pre-gate parity with the callers it replaced).
pub fn begin(grok_com_config: Option<GrokComConfig>) -> bool {
    begin_inner(|| resolve_disk_auth(grok_com_config))
}

/// [`begin`] with pre-resolved auth; a registered fetch wins and `auth` is dropped.
pub fn begin_with_auth(auth: Option<GrokAuth>) -> bool {
    begin_inner(move || auth)
}

/// True when a fetch is in flight after the call (started or joined).
/// Auth is lazy so tests reach the guard without touching disk.
fn begin_inner(auth: impl FnOnce() -> Option<GrokAuth>) -> bool {
    if cfg!(test) {
        return INFLIGHT.lock().unwrap().is_some();
    }
    if INFLIGHT.lock().unwrap().is_some() {
        return true;
    }
    // Reads disk; stay outside the registry lock.
    let Some(env) = prefetch_env(auth()) else {
        return false;
    };
    let mut inflight = INFLIGHT.lock().unwrap();
    if inflight.is_some() {
        return true;
    }
    let cell = Arc::new(Inflight {
        origin: env.endpoints.proxy_url(),
        state: Mutex::new(State::default()),
        done: Condvar::new(),
    });
    let worker_cell = cell.clone();
    std::thread::spawn(move || {
        let _guard = FinishGuard(worker_cell.clone());
        let (models, settings, settings_write) = super::run_prefetch(env);
        let mut state = worker_cell.state.lock().unwrap();
        state.settings = settings;
        state.models_write = models.into_deferred_write();
        state.settings_write = settings_write;
    });
    *inflight = Some(cell);
    true
}

/// Proof the worker finished: the only key that removes the registry entry,
/// so a live worker can never be deregistered and a timed-out fetch stays
/// behind as a tombstone.
struct Finished(Arc<Inflight>);

fn wait_finished(cell: Arc<Inflight>, deadline: Duration) -> Option<Finished> {
    let (state, wait) = cell
        .done
        .wait_timeout_while(cell.state.lock().unwrap(), deadline, |s| !s.finished)
        .unwrap();
    drop(state);
    if wait.timed_out() {
        return None;
    }
    Some(Finished(cell))
}

impl Finished {
    /// Remove the registry entry and yield the worker's result.
    fn take(self) -> State {
        let mut registry = INFLIGHT.lock().unwrap();
        if registry.as_ref().is_some_and(|c| Arc::ptr_eq(c, &self.0)) {
            registry.take();
        }
        drop(registry);
        std::mem::take(&mut *self.0.state.lock().unwrap())
    }
}

/// Clone the settings once ready, leaving the fetch registered for
/// `bootstrap` to consume. Read-only.
pub fn wait_settings(timeout: Duration) -> Option<RemoteSettings> {
    let cell = INFLIGHT.lock().unwrap().clone()?;
    if !still_accepted(&cell.origin) {
        return None;
    }
    let finished = wait_finished(cell, timeout)?;
    if !still_accepted(&finished.0.origin) {
        return None;
    }
    finished.0.state.lock().unwrap().settings.clone()
}

pub(crate) enum Accept {
    /// A fetch was consumed. The boot's settings budget is spent even when it
    /// carried no settings: an empty fetch must not trigger a second
    /// pre-first-screen retry sequence (#278686).
    Consumed(Option<Box<RemoteSettings>>),
    /// Nothing usable was in flight; the caller may fetch under current policy.
    Miss,
}

/// The only commit point: consume the fetch, re-check policy, then persist and yield.
pub(crate) fn accept() -> Accept {
    accept_with_deadline(ACCEPT_DEADLINE)
}

fn accept_with_deadline(deadline: Duration) -> Accept {
    let Some(cell) = INFLIGHT.lock().unwrap().clone() else {
        return Accept::Miss;
    };
    let origin = cell.origin.clone();
    let Some(finished) = wait_finished(cell, deadline) else {
        // Budget spent: retrying would double the pre-first-screen fetches.
        tracing::warn!("settings prefetch outlived its deadline; booting without settings");
        return Accept::Consumed(None);
    };
    let mut state = finished.take();
    if state.panicked {
        tracing::warn!("settings prefetch thread panicked");
    }
    let settings = state.settings.take();
    let models_write = state.models_write.take();
    let settings_write = state.settings_write.take();
    if !still_accepted(&origin) {
        return Accept::Miss;
    }
    if let Some(write) = models_write {
        write.commit();
    }
    if let Some(write) = settings_write {
        write.commit();
    }
    if settings.is_none() {
        tracing::info!("settings prefetch returned no settings");
    }
    Accept::Consumed(settings.map(Box::new))
}

/// One blocking fetch under the rules of [`begin_before_policy_gate`].
pub(crate) fn fetch_now_before_policy_gate(cfg: &Config) -> Option<RemoteSettings> {
    if !begin_before_policy_gate(cfg) {
        return None;
    }
    consume()
}

fn consume() -> Option<RemoteSettings> {
    match accept() {
        Accept::Consumed(settings) => settings.map(|s| *s),
        Accept::Miss => None,
    }
}

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn clear_for_tests() {
    INFLIGHT.lock().unwrap().take();
}

fn still_accepted(origin: &str) -> bool {
    if !crate::util::config::resolve_remote_fetch_enabled() {
        tracing::info!("startup prefetch discarded: remote_fetch disabled");
        return false;
    }
    if origin != resolve_startup_endpoints().proxy_url() {
        tracing::info!("startup prefetch discarded: fetch origin changed");
        return false;
    }
    true
}

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn inject_for_tests(settings: Option<RemoteSettings>) {
    inject_with_origin_for_tests(settings, resolve_startup_endpoints().proxy_url());
}

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn inject_with_origin_for_tests(settings: Option<RemoteSettings>, origin: String) {
    let cell = Arc::new(Inflight {
        origin,
        state: Mutex::new(State {
            finished: true,
            panicked: false,
            settings,
            models_write: None,
            settings_write: None,
        }),
        done: Condvar::new(),
    });
    *INFLIGHT.lock().unwrap() = Some(cell);
}

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn inflight_for_tests() -> bool {
    INFLIGHT.lock().unwrap().is_some()
}

#[cfg(test)]
#[path = "startup_prefetch_tests.rs"]
mod tests;
