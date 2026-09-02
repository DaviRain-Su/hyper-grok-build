//! Sync `managed_config.toml` + `requirements.toml` from the deployment-config endpoint per
//! principal; evicted on identity switch and cleared on logout, so config never crosses principals.

mod policy;
mod response;
mod store;
mod supervisor;

pub use policy::ManagedPolicyRefusal;
pub use response::ManagedConfigError;
// Test seams: pub only so the integration suites can link them.
#[doc(hidden)]
pub use store::MANAGED_ARTIFACT_FILES;
pub use store::{
    classify_auth_mode, clear_orphan, current_serving_identity, has_principal, is_fetch_enabled,
};
pub(crate) use store::{resolve_deployment_id, resolve_deployment_key};
pub(crate) use supervisor::policy_repair_pending;
#[doc(hidden)]
pub use supervisor::{ManagedConfigRefresher, spawn_refresh_supervisor, take_refresh_supervisor};
pub use supervisor::{
    ManagedConfigSync, SetupOutcome, SetupReport, ensure_managed_policy_present,
    fetch_setup_report, post_login_sync, run_setup, start_refresh_supervisor, sync,
};

/// Absorbs a healthy in-flight apply without letting a wedged holder stall start.
const GATE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Fail-closed session-start gate for managed principals.
pub fn managed_policy_gate() -> Result<(), ManagedPolicyRefusal> {
    managed_policy_gate_with_lock_wait(GATE_LOCK_WAIT)
}

/// Test seam injecting the bounded lock wait; production stays on [`GATE_LOCK_WAIT`].
#[doc(hidden)]
pub fn managed_policy_gate_with_lock_wait(
    lock_wait: std::time::Duration,
) -> Result<(), ManagedPolicyRefusal> {
    // Unit tests would hit the host's real home.
    if cfg!(test) {
        return Ok(());
    }
    if !store::managed_principal_present() {
        return Ok(());
    }
    let snapshot = locked_gate_snapshot(&crate::util::grok_home::grok_home(), lock_wait)?;
    policy::managed_policy_gate_decision(snapshot)
}

fn locked_gate_snapshot(
    home: &std::path::Path,
    lock_wait: std::time::Duration,
) -> Result<policy::GateSnapshot, ManagedPolicyRefusal> {
    let lock_file = match store::try_gate_lock(home) {
        store::GateLockAttempt::Acquired(lock_file) => lock_file,
        // block_in_place lets a multi-thread runtime backfill the worker; plain parking is
        // safe on a current-thread one (no task ever holds the flock across an await).
        store::GateLockAttempt::Contended(lock_file) => {
            match tokio::runtime::Handle::try_current() {
                Ok(handle)
                    if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread =>
                {
                    tokio::task::block_in_place(|| {
                        store::wait_for_gate_lock(&lock_file, home, lock_wait)
                    })?
                }
                _ => store::wait_for_gate_lock(&lock_file, home, lock_wait)?,
            }
            lock_file
        }
        store::GateLockAttempt::Unavailable => {
            return Err(ManagedPolicyRefusal::LockUnavailable {
                home: home.to_path_buf(),
            });
        }
    };
    let _lock = lock_file;
    Ok(store::gate_snapshot_locked(home))
}

#[cfg(test)]
mod tests;
