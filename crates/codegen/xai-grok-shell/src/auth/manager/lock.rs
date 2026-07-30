//! Advisory `auth.json.lock` helpers (free functions, no `AuthManager`
//! dependency).
//!
//! Uses flock + PID-in-file + unlink-to-break for robust stale-lock
//! recovery:
//! - `flock(LOCK_EX | LOCK_NB)` for race-free mutual exclusion
//! - `PID:TIMESTAMP` written into the lock file so waiters can detect
//!   staleness
//! - Waiters that find a dead or stuck holder `unlink` the lock file
//!   and retry on a fresh inode (the old holder's flock lives on the
//!   now-unlinked inode)

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::path::Path;
use std::time::Duration as StdDuration;

use fs2::FileExt;

use crate::auth::storage::AuthFileLock;
use crate::unified_log;

/// Maximum age (seconds) of a lock holder before it is considered stuck.
const STALE_LOCK_TIMEOUT_SECS: u64 = 60;

/// How long a **live** holder must stay stale under re-observation of fresh
/// *awake* time before a waiter may break its lock. Staleness is wall-clock,
/// which keeps counting through a suspend — at wake every lock held across
/// it reads "stuck ≥ 60 s" even though its (alive) holder re-dates itself
/// within one [`LOCK_HEARTBEAT_INTERVAL`]. Breaking at wake+0 s double-spends
/// the refresh token the holder already sent, revoking the token family.
/// Dead holders (PID gone) are still broken immediately.
const STUCK_LIVE_CONFIRM_DELAY: StdDuration = StdDuration::from_secs(12);

/// Cadence of holder-info rewrites while a lock is held (see [`LockHeartbeat`]).
pub(crate) const LOCK_HEARTBEAT_INTERVAL: StdDuration = StdDuration::from_secs(5);

// A woken live holder must always re-date itself (≤ one heartbeat interval,
// plus scheduling slack) before a waiter's confirmation window elapses.
const _: () = assert!(
    STUCK_LIVE_CONFIRM_DELAY.as_millis() >= 2 * LOCK_HEARTBEAT_INTERVAL.as_millis(),
    "confirmation delay must comfortably exceed one heartbeat interval"
);
const _: () = assert!(
    LOCK_HEARTBEAT_INTERVAL.as_secs() < STALE_LOCK_TIMEOUT_SECS,
    "a heartbeating holder must never age past the stale threshold"
);
// Refresh-sized callers must keep both a meaningful Phase-2 wait after the
// Phase-3 confirmation reservation ([`phase2_budget`]) and the heartbeat
// (the `timeout >= REFRESH_LOCK_TIMEOUT` gate in
// [`try_lock_auth_file_async_with`]).
const _: () = assert!(
    super::REFRESH_LOCK_TIMEOUT.as_millis() >= 2 * STUCK_LIVE_CONFIRM_DELAY.as_millis(),
    "the refresh lock budget must comfortably exceed the confirmation delay"
);

/// Background thread that re-dates the lock file's holder info (`PID:TS`)
/// every [`LOCK_HEARTBEAT_INTERVAL`] while an [`AuthFileLock`] is held, so a
/// holder suspended across sleep stops reading as "stuck" within one
/// interval of waking. Writes through a `try_clone`d FD (same open file
/// description; flock unaffected); Drop stops and joins the thread.
pub(crate) struct LockHeartbeat {
    stop: std::sync::mpsc::Sender<()>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl LockHeartbeat {
    fn spawn(mut file: File, interval: StdDuration) -> Self {
        let (stop, ticks) = std::sync::mpsc::channel::<()>();
        let handle = std::thread::Builder::new()
            .name("auth-lock-heartbeat".into())
            .spawn(move || {
                // Timeout = keep beating; Ok(()) or Disconnected = stop.
                while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                    ticks.recv_timeout(interval)
                {
                    if let Err(e) = write_holder_info(&mut file) {
                        // Best-effort: a failed rewrite leaves the previous
                        // holder info in place; the waiter-side confirmation
                        // delay still protects a live holder.
                        tracing::debug!(error = %e, "auth lock: heartbeat rewrite failed");
                    }
                }
            })
            .ok();
        Self { stop, handle }
    }
}

impl Drop for LockHeartbeat {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

// ── Holder-info helpers ──────────────────────────────────────────────

/// Write `PID:UNIX_TIMESTAMP` into the lock file so waiters can detect
/// staleness.
fn write_holder_info(file: &mut File) -> io::Result<()> {
    let pid = std::process::id();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    file.set_len(0)?;
    file.seek(io::SeekFrom::Start(0))?;
    write!(file, "{pid}:{ts}")?;
    file.sync_all()?;
    Ok(())
}

/// Parse `PID:UNIX_TIMESTAMP` from lock file content.
fn parse_holder_info(content: &str) -> Option<(u32, u64)> {
    let (pid_str, ts_str) = content.trim().split_once(':')?;
    Some((pid_str.parse().ok()?, ts_str.parse().ok()?))
}

// ── Platform-specific helpers ────────────────────────────────────────

/// Check whether the process that wrote the lock file is still running.
#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    // `pid_t` is `i32`; values ≤ 0 have special semantics for `kill(2)`
    // (0 = own process group, -1 = all processes).  Reject them so we
    // don't accidentally probe the wrong target.
    let pid_i = match i32::try_from(pid) {
        Ok(p) if p > 0 => p,
        _ => return false,
    };
    // SAFETY: `kill(pid, 0)` is a POSIX-defined no-op signal used solely
    // for existence testing.
    //   ret ==  0          → process exists and we can signal it
    //   ret == -1, ESRCH   → process does not exist
    //   ret == -1, EPERM   → process exists but we lack permission
    // We must treat EPERM as "alive" to avoid breaking a live holder's
    // lock when running under a different effective UID.
    let ret = unsafe { libc::kill(pid_i as libc::pid_t, 0) };
    if ret == 0 {
        return true;
    }
    // errno == ESRCH means the process is gone; any other errno
    // (e.g. EPERM) means it exists but we can't signal it.
    let err = io::Error::last_os_error();
    err.raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn is_process_alive(_pid: u32) -> bool {
    true // conservative fallback — skip liveness check on non-Unix
}

/// `fstat(fd)` vs `stat(path)` inode comparison.  Detects a concurrent
/// unlink+recreate between our `flock` and the subsequent check.
#[cfg(unix)]
fn inodes_match(file: &File, path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let fd_meta = file.metadata()?;
    let path_meta = std::fs::metadata(path)?;
    Ok(fd_meta.ino() == path_meta.ino() && fd_meta.dev() == path_meta.dev())
}

#[cfg(not(unix))]
fn inodes_match(_file: &File, _path: &Path) -> io::Result<bool> {
    Ok(true) // no inode concept; skip the check
}

// ── Staleness check ──────────────────────────────────────────────────

/// Decide staleness when the lock file carries no usable `PID:TS` holder
/// info — it is empty, was truncated mid-write, or holds non-UTF-8
/// garbage. We have no PID to liveness-probe, so we fall back to the lock
/// file's mtime: every real holder rewrites holder info (bumping mtime)
/// the instant it takes the flock, so a lock file whose mtime is older
/// than [`STALE_LOCK_TIMEOUT_SECS`] has been abandoned and is safe to
/// break. A lock caught in the sub-millisecond `set_len(0)`→write window
/// keeps a fresh mtime and is therefore never broken by this path.
///
/// Returning `false` here used to be unconditional ("assume alive"), which
/// turned a single empty/garbage lock file into an unbreakable lock and
/// wedged every refresh behind it.
fn unidentified_holder_is_stale(file: &File, why: &str) -> bool {
    let Ok(modified) = file.metadata().and_then(|m| m.modified()) else {
        unified_log::debug(
            &format!("auth lock: {why}; mtime unreadable, assuming alive"),
            None,
            None,
        );
        return false;
    };
    let age = modified.elapsed().unwrap_or_default().as_secs();
    if age > STALE_LOCK_TIMEOUT_SECS {
        unified_log::info(
            &format!(
                "auth lock: {why}; mtime age={age}s > {STALE_LOCK_TIMEOUT_SECS}s, breaking stale lock"
            ),
            None,
            Some(serde_json::json!({ "age_secs": age, "threshold_secs": STALE_LOCK_TIMEOUT_SECS })),
        );
        true
    } else {
        unified_log::debug(
            &format!("auth lock: {why}; mtime age={age}s within threshold, assuming alive"),
            None,
            None,
        );
        false
    }
}

/// Classification of the current lock holder, driving how aggressively a
/// waiter may break the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HolderState {
    /// Holder process is gone — its flock died with it; break immediately.
    Dead,
    /// Alive but holder info (or mtime, when unidentifiable) is past
    /// [`STALE_LOCK_TIMEOUT_SECS`]: genuinely wedged, or just woke from a
    /// suspend and hasn't re-dated yet. Breakable only per
    /// [`StuckLivePolicy`].
    StuckLive,
    /// Holder looks healthy — wait.
    Alive,
}

/// Read the lock file content and classify the current holder: process dead,
/// live-but-stale (holder info older than [`STALE_LOCK_TIMEOUT_SECS`], or
/// unidentifiable holder info with an mtime past the threshold), or alive.
fn holder_state(file: &mut File) -> HolderState {
    let mut content = String::new();
    if file.seek(io::SeekFrom::Start(0)).is_err() || file.read_to_string(&mut content).is_err() {
        return if unidentified_holder_is_stale(file, "holder info unreadable") {
            // No PID to probe: we cannot distinguish dead from suspended, so
            // classify as stuck-live and let the confirmation-delay policy
            // decide (the pre-heartbeat behavior broke these immediately).
            HolderState::StuckLive
        } else {
            HolderState::Alive
        };
    }
    let Some((holder_pid, holder_ts)) = parse_holder_info(&content) else {
        return if unidentified_holder_is_stale(
            file,
            &format!("holder info unparseable (raw={content:?})"),
        ) {
            HolderState::StuckLive
        } else {
            HolderState::Alive
        };
    };

    // Process dead?
    if !is_process_alive(holder_pid) {
        unified_log::info(
            &format!("auth lock: holder pid={holder_pid} is dead, breaking stale lock"),
            None,
            Some(serde_json::json!({ "holder_pid": holder_pid, "holder_ts": holder_ts })),
        );
        return HolderState::Dead;
    }

    // Process stuck (holding > STALE_LOCK_TIMEOUT_SECS)?
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let age = now.saturating_sub(holder_ts);
    if age > STALE_LOCK_TIMEOUT_SECS {
        unified_log::info(
            &format!(
                "auth lock: holder pid={holder_pid} appears stuck (age={age}s > {STALE_LOCK_TIMEOUT_SECS}s)"
            ),
            None,
            Some(
                serde_json::json!({ "holder_pid": holder_pid, "age_secs": age, "threshold_secs": STALE_LOCK_TIMEOUT_SECS }),
            ),
        );
        return HolderState::StuckLive;
    }

    HolderState::Alive
}

// ── Single-iteration acquire logic ───────────────────────────────────

/// Outcome of one lock attempt.
enum LockAttempt {
    /// Lock acquired; inner file holds the flock.
    Acquired(File),
    /// Lock is legitimately held by another live process — sleep and retry.
    Busy,
    /// Stale lock was unlinked — retry immediately on a fresh inode.
    StaleUnlinked,
    /// Unrecoverable I/O error — give up.
    Failed,
}

/// How a lock attempt treats a holder that is alive but stale
/// ([`HolderState::StuckLive`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StuckLivePolicy {
    /// Never break a live holder on first sight — at wake every suspended
    /// holder reads stale (see [`STUCK_LIVE_CONFIRM_DELAY`]).
    Wait,
    /// The caller re-observed the holder still stale after the confirmation
    /// delay of awake time: genuinely wedged — break.
    Break,
}

/// Execute one iteration of the acquire loop.
///
/// `lock_path` is the resolved path to `auth.json.lock` — computed once
/// by the caller to avoid re-deriving it on every poll iteration.
fn try_acquire_once(lock_path: &Path, stuck_live: StuckLivePolicy) -> LockAttempt {
    // Step 1: open (create if missing) auth.json.lock
    let mut file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
    {
        Ok(f) => f,
        Err(e) => {
            unified_log::warn(
                &format!("auth lock: failed to open {}: {e}", lock_path.display()),
                None,
                None,
            );
            return LockAttempt::Failed;
        }
    };

    // Step 2: flock(LOCK_EX | LOCK_NB)
    match file.try_lock_exclusive() {
        Ok(()) => {
            let pid = std::process::id();
            // Step 3: write holder info, then verify same inode.
            if let Err(e) = write_holder_info(&mut file) {
                unified_log::warn(
                    &format!("auth lock: failed to write holder info: {e}"),
                    None,
                    Some(serde_json::json!({ "pid": pid })),
                );
                // Still hold the flock — proceed
            }

            match inodes_match(&file, lock_path) {
                Ok(true) => {
                    unified_log::debug(
                        &format!("auth lock: acquired (pid={pid})"),
                        None,
                        Some(
                            serde_json::json!({ "pid": pid, "path": lock_path.display().to_string() }),
                        ),
                    );
                    LockAttempt::Acquired(file)
                }
                Ok(false) => {
                    // Someone else unlinked our file and created a new one;
                    // our flock is on the deleted inode.  Retry.
                    unified_log::debug(
                        &format!("auth lock: inode changed after acquire (pid={pid}), retrying"),
                        None,
                        None,
                    );
                    LockAttempt::StaleUnlinked
                }
                Err(e) => {
                    // Path deleted between flock and stat — retry.
                    unified_log::debug(
                        &format!("auth lock: path gone after acquire (pid={pid}): {e}"),
                        None,
                        None,
                    );
                    LockAttempt::StaleUnlinked
                }
            }
        }

        // Step 4: EWOULDBLOCK — lock is held by someone else.
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
            let breakable = match holder_state(&mut file) {
                HolderState::Dead => true,
                HolderState::StuckLive => match stuck_live {
                    StuckLivePolicy::Break => true,
                    StuckLivePolicy::Wait => {
                        unified_log::info(
                            "auth lock: holder live but stale; deferring break until after the blocking wait",
                            None,
                            None,
                        );
                        false
                    }
                },
                HolderState::Alive => false,
            };
            if breakable {
                match std::fs::remove_file(lock_path) {
                    Ok(()) => LockAttempt::StaleUnlinked,
                    Err(e) => {
                        // Unlink failed (permissions, etc.) — fall back to
                        // Busy so the caller sleeps before retrying instead
                        // of tight-looping on repeated unlink failures.
                        unified_log::warn(
                            &format!("auth lock: failed to unlink stale lock file: {e}"),
                            None,
                            None,
                        );
                        LockAttempt::Busy
                    }
                }
            } else {
                LockAttempt::Busy
            }
        }

        Err(e) => {
            unified_log::warn(&format!("auth lock: flock failed: {e}"), None, None);
            LockAttempt::Failed
        }
    }
}

// ── Blocking acquire (kernel FIFO wait queue) ────────────────────────

/// Attempt a blocking `flock(LOCK_EX)` on the lock file.  Returns the
/// locked file on success, or an error on I/O failure / inode mismatch.
///
/// This blocks the calling thread in the kernel's flock wait queue until
/// the lock is available — FIFO-fair, zero CPU while waiting.
fn blocking_acquire(lock_path: &Path) -> io::Result<File> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;

    // Blocking flock — waits in kernel until the lock is available.
    file.lock_exclusive().map_err(|e| {
        unified_log::warn(
            &format!("auth lock: blocking flock failed: {e}"),
            None,
            None,
        );
        e
    })?;

    let pid = std::process::id();
    if let Err(e) = write_holder_info(&mut file) {
        unified_log::warn(
            &format!("auth lock: failed to write holder info: {e}"),
            None,
            Some(serde_json::json!({ "pid": pid })),
        );
        // Still hold the flock — proceed.
    }

    // Verify the FD's inode still matches the path (detects a concurrent
    // unlink+recreate that happened between our open and our flock).
    match inodes_match(&file, lock_path) {
        Ok(true) => {
            unified_log::debug(
                &format!("auth lock: acquired via blocking flock (pid={pid})"),
                None,
                Some(serde_json::json!({ "pid": pid, "path": lock_path.display().to_string() })),
            );
            Ok(file)
        }
        Ok(false) => Err(io::Error::other(
            "inode changed during blocking flock (concurrent unlink+recreate)",
        )),
        Err(e) => Err(io::Error::other(format!(
            "path gone after blocking flock: {e}"
        ))),
    }
}

// ── Public API ───────────────────────────────────────────────────────

/// Best-effort **non-blocking** acquire for advisory cleanup call sites
/// (`AuthManager::new` WebLogin cleanup, `remove_scope`).
///
/// Unlike [`try_lock_auth_file_async`] this never waits and never breaks a
/// stale lock: it takes the flock iff it is free right now, otherwise
/// returns `None` so the caller simply skips its best-effort write.
/// Crucially it records `PID:TS` holder info after locking, so a waiter
/// that observes the flock can identify the holder (and break it once
/// stale). Taking the flock *without* writing holder info is what used to
/// leave an empty `auth.json.lock` that defeated stale-lock recovery.
pub(crate) fn try_lock_auth_file_nonblocking(auth_json_path: &Path) -> Option<AuthFileLock> {
    let lock_path = auth_json_path.with_file_name("auth.json.lock");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .ok()?;

    // Non-blocking: bail immediately if another holder has the flock.
    file.try_lock_exclusive().ok()?;

    if let Err(e) = write_holder_info(&mut file) {
        unified_log::warn(
            &format!("auth lock: failed to write holder info (non-blocking): {e}"),
            None,
            Some(serde_json::json!({ "pid": std::process::id() })),
        );
        // Still hold the flock — proceed.
    }
    // No heartbeat: these advisory holds are millisecond-scale cleanup
    // writes, not IdP exchanges worth protecting across a suspend.
    Some(AuthFileLock {
        _heartbeat: None,
        _file: file,
    })
}

/// Wrap a freshly flocked `file` in the RAII guard, optionally with the
/// heartbeat attached (heartbeat-less if the FD cannot be cloned).
/// `with_heartbeat` is `false` for short advisory holds: they don't span an
/// IdP exchange (no double-spend to protect) and run several times per
/// second, so a thread per acquisition is real overhead — same reasoning as
/// [`try_lock_auth_file_nonblocking`].
fn locked(file: File, with_heartbeat: bool) -> AuthFileLock {
    if !with_heartbeat {
        return AuthFileLock {
            _heartbeat: None,
            _file: file,
        };
    }
    let heartbeat = match file.try_clone() {
        Ok(clone) => Some(LockHeartbeat::spawn(clone, LOCK_HEARTBEAT_INTERVAL)),
        Err(e) => {
            unified_log::warn(
                &format!("auth lock: failed to clone FD for heartbeat: {e}"),
                None,
                None,
            );
            None
        }
    };
    AuthFileLock {
        _heartbeat: heartbeat,
        _file: file,
    }
}

/// Acquire the `auth.json.lock` file lock with three phases:
///
/// 1. **Instant try** — non-blocking `flock(LOCK_NB)`.  Succeeds
///    immediately if the lock is free.
/// 2. **Blocking wait** — `flock(LOCK_EX)` on a `spawn_blocking`
///    thread, wrapped in `tokio::time::timeout`.  The kernel's flock
///    wait queue is FIFO: when the holder releases, exactly one waiter
///    wakes.  Zero CPU while waiting.
/// 3. **Stale fallback** — after the blocking wait, break a dead holder
///    immediately; a live-but-stale one only after re-observing it still
///    stale across [`STUCK_LIVE_CONFIRM_DELAY`] of fresh awake time
///    (skipped when `timeout` is below the delay).
///
/// `timeout` is the total budget: Phase 2's wait is shortened by
/// [`phase2_budget`] so the function returns within ~`timeout`, never
/// `timeout + STUCK_LIVE_CONFIRM_DELAY`. The guard carries the holder
/// heartbeat only for refresh-sized budgets (see [`locked`]).
pub(crate) async fn try_lock_auth_file_async(
    auth_json_path: &Path,
    timeout: StdDuration,
) -> Option<AuthFileLock> {
    try_lock_auth_file_async_with(auth_json_path, timeout, STUCK_LIVE_CONFIRM_DELAY).await
}

/// Phase-2 (blocking-wait) budget: the total `timeout` minus a reservation
/// for Phase 3's confirmation sleep, so total wall time stays within the
/// caller's budget. Budgets below the delay never confirm, so they keep
/// everything for Phase 2.
fn phase2_budget(timeout: StdDuration, confirm_delay: StdDuration) -> StdDuration {
    if timeout >= confirm_delay {
        timeout - confirm_delay
    } else {
        timeout
    }
}

/// [`try_lock_auth_file_async`] with the stuck-live confirmation delay
/// injectable, so tests exercise the Phase-3 policy without production-sized
/// waits. `confirm_delay` must exceed the heartbeat interval to keep the
/// double-spend guarantee (the production constant is const-asserted).
async fn try_lock_auth_file_async_with(
    auth_json_path: &Path,
    timeout: StdDuration,
    confirm_delay: StdDuration,
) -> Option<AuthFileLock> {
    let lock_path = auth_json_path.with_file_name("auth.json.lock");
    // Heartbeat only for holds that may span an IdP exchange; see
    // [`locked`] for why short advisory holds skip it.
    let with_heartbeat = timeout >= super::REFRESH_LOCK_TIMEOUT;

    unified_log::debug(
        &format!(
            "auth lock: attempting acquire (timeout={}ms)",
            timeout.as_millis()
        ),
        None,
        Some(
            serde_json::json!({ "path": lock_path.display().to_string(), "timeout_ms": timeout.as_millis() as u64 }),
        ),
    );

    // Phase 1: instant non-blocking try (StuckLivePolicy::Wait — see
    // STUCK_LIVE_CONFIRM_DELAY; dead holders are still broken).
    match try_acquire_once(&lock_path, StuckLivePolicy::Wait) {
        LockAttempt::Acquired(file) => return Some(locked(file, with_heartbeat)),
        LockAttempt::Failed => return None,
        LockAttempt::StaleUnlinked | LockAttempt::Busy => { /* fall through to Phase 2 */ }
    }

    // Phase 2: blocking flock via spawn_blocking + timeout. The wait is
    // capped at `phase2_budget` (not the full `timeout`) so Phase 3's
    // confirmation sleep fits inside the caller's total budget.
    // Retry loop handles the rare inode-mismatch race (a third process
    // unlinked the lock file between our open and our flock).
    let wait_budget = phase2_budget(timeout, confirm_delay);
    let deadline = tokio::time::Instant::now() + wait_budget;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining == StdDuration::ZERO {
            break; // fall through to Phase 3
        }

        let lp = lock_path.clone();
        let result = tokio::time::timeout(
            remaining,
            tokio::task::spawn_blocking(move || blocking_acquire(&lp)),
        )
        .await;

        match result {
            // Blocking flock succeeded, inode matches.
            Ok(Ok(Ok(file))) => return Some(locked(file, with_heartbeat)),
            // Inode mismatch — retry from the top of the loop.
            Ok(Ok(Err(_inode_err))) => continue,
            // spawn_blocking panicked — give up.
            Ok(Err(_join_err)) => return None,
            // Timeout — fall through to Phase 3.
            Err(_timeout) => break,
        }
    }

    // Phase 3: stale-lock recovery (last resort). Dead holders break
    // immediately; a live-but-stale one only after re-observation across
    // `confirm_delay` of *fresh awake* time — Phase 2's monotonic wait may
    // have elapsed before a suspend, leaving a woken holder no awake time
    // to heartbeat. tokio's timer pauses during suspend, so the sleep below
    // measures awake time by construction.
    unified_log::warn(
        &format!(
            "auth lock: blocking flock timed out after {}ms, trying stale recovery",
            wait_budget.as_millis()
        ),
        None,
        Some(serde_json::json!({
            "path": lock_path.display().to_string(),
            "timeout_ms": timeout.as_millis() as u64,
            "phase2_budget_ms": wait_budget.as_millis() as u64,
        })),
    );
    let mut saw_stuck_live = false;
    for _ in 0..2 {
        match try_acquire_once(&lock_path, StuckLivePolicy::Wait) {
            LockAttempt::Acquired(file) => return Some(locked(file, with_heartbeat)),
            LockAttempt::StaleUnlinked => continue, // dead holder broken; retry
            LockAttempt::Busy => {
                saw_stuck_live = true; // alive-fresh or stuck-live; confirm below
                break;
            }
            LockAttempt::Failed => break,
        }
    }

    if saw_stuck_live && timeout >= confirm_delay {
        tokio::time::sleep(confirm_delay).await;
        for _ in 0..2 {
            match try_acquire_once(&lock_path, StuckLivePolicy::Break) {
                LockAttempt::Acquired(file) => return Some(locked(file, with_heartbeat)),
                LockAttempt::StaleUnlinked => continue,
                LockAttempt::Busy | LockAttempt::Failed => break,
            }
        }
    }

    unified_log::warn(
        &format!(
            "auth lock: all phases exhausted after {}ms",
            timeout.as_millis()
        ),
        None,
        Some(
            serde_json::json!({ "path": lock_path.display().to_string(), "timeout_ms": timeout.as_millis() as u64 }),
        ),
    );
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn auth_json_path(dir: &TempDir) -> std::path::PathBuf {
        dir.path().join("auth.json")
    }

    // ── Pure-function unit tests (no runtime needed) ─────────────────

    #[test]
    fn test_write_and_parse_holder_info() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("test.lock");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&lock_path)
            .unwrap();

        write_holder_info(&mut file).unwrap();

        file.seek(io::SeekFrom::Start(0)).unwrap();
        let mut content = String::new();
        file.read_to_string(&mut content).unwrap();

        let (pid, ts) = parse_holder_info(&content).unwrap();
        assert_eq!(pid, std::process::id());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(now - ts < 2, "timestamp should be within 2 seconds");
    }

    #[test]
    fn test_parse_holder_info_edge_cases() {
        assert_eq!(
            parse_holder_info("12345:1700000000"),
            Some((12345, 1700000000))
        );
        assert_eq!(
            parse_holder_info("  12345:1700000000  "),
            Some((12345, 1700000000))
        );
        assert!(parse_holder_info("").is_none());
        assert!(parse_holder_info("no-colon").is_none());
        assert!(parse_holder_info("abc:123").is_none());
        assert!(parse_holder_info("123:abc").is_none());
    }

    #[test]
    fn test_unidentified_holder_is_stale_by_mtime() {
        // An empty / unparseable lock file is broken based on mtime: fresh
        // means a holder may be mid-write (assume alive), old means it was
        // abandoned (break it). Regression for the production wedge where
        // an empty `auth.json.lock` was treated as alive forever.
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("test.lock");
        std::fs::write(&lock_path, b"").unwrap(); // empty → unparseable

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();

        assert!(
            !unidentified_holder_is_stale(&file, "test"),
            "fresh empty lock must be assumed alive"
        );

        let old = filetime::FileTime::from_unix_time(
            (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64)
                - (STALE_LOCK_TIMEOUT_SECS as i64 + 30),
            0,
        );
        filetime::set_file_mtime(&lock_path, old).unwrap();

        assert!(
            unidentified_holder_is_stale(&file, "test"),
            "empty lock older than the stale threshold must be broken"
        );
    }

    #[test]
    fn test_nonblocking_acquire_writes_holder_info() {
        // fix: advisory cleanup sites must record `PID:TS`, never hold the
        // flock over an empty lock file.
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);
        let lock_path = path.with_file_name("auth.json.lock");

        let lock = try_lock_auth_file_nonblocking(&path).expect("uncontended non-blocking acquire");

        let content = std::fs::read_to_string(&lock_path).unwrap();
        let (pid, _ts) =
            parse_holder_info(&content).expect("non-blocking acquire must write parseable info");
        assert_eq!(pid, std::process::id());

        drop(lock);
    }

    #[test]
    fn test_nonblocking_acquire_returns_none_when_held() {
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);

        let lock1 = try_lock_auth_file_nonblocking(&path).expect("first acquire");
        // Same process, different FD → WouldBlock. Non-blocking acquire
        // must not wait and must not break a live lock.
        let lock2 = try_lock_auth_file_nonblocking(&path);
        assert!(lock2.is_none(), "must return None when the lock is held");
        drop(lock1);
    }

    #[cfg(unix)]
    #[test]
    fn test_is_process_alive() {
        assert!(is_process_alive(std::process::id()));
        assert!(!is_process_alive(0));
        assert!(!is_process_alive(u32::MAX));
        assert!(!is_process_alive(i32::MAX as u32));
    }

    #[cfg(unix)]
    #[test]
    fn test_is_holder_stale_dead_pid() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("test.lock");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&lock_path)
            .unwrap();

        let dead_pid: u32 = i32::MAX as u32;
        write!(file, "{dead_pid}:9999999999").unwrap();
        file.sync_all().unwrap();

        assert_eq!(
            holder_state(&mut file),
            HolderState::Dead,
            "dead PID should classify as Dead (immediately breakable)"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_is_holder_stale_alive_pid() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("test.lock");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&lock_path)
            .unwrap();

        write_holder_info(&mut file).unwrap();

        assert_eq!(
            holder_state(&mut file),
            HolderState::Alive,
            "live process with recent timestamp should not be stale"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_is_holder_stale_old_timestamp() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("test.lock");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&lock_path)
            .unwrap();

        let our_pid = std::process::id();
        let old_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 200;
        write!(file, "{our_pid}:{old_ts}").unwrap();
        file.sync_all().unwrap();

        assert_eq!(
            holder_state(&mut file),
            HolderState::StuckLive,
            "live PID with old timestamp classifies StuckLive, not immediately breakable"
        );
    }

    /// The suspend-straddle double-spend guard end-to-end: a LIVE holder whose
    /// holder info is stale (the on-disk state every suspended holder shows at
    /// wake) must NOT be broken by the instant path, and a short-timeout
    /// waiter (below the confirmation delay) must give up rather than break.
    #[cfg(unix)]
    #[tokio::test]
    async fn stuck_live_holder_not_broken_at_first_sight() {
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);
        let lock_path = path.with_file_name("auth.json.lock");

        // Hold the flock on a separate FD, with holder info backdated past
        // the stale threshold — a live process that "slept" 200 s.
        let mut holder = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&lock_path)
            .unwrap();
        holder.try_lock_exclusive().unwrap();
        let old_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 200;
        write!(holder, "{}:{old_ts}", std::process::id()).unwrap();
        holder.sync_all().unwrap();

        // Instant path: must classify Busy (defer), not unlink.
        assert!(
            matches!(
                try_acquire_once(&lock_path, StuckLivePolicy::Wait),
                LockAttempt::Busy
            ),
            "live-but-stale holder must be Busy under Wait policy"
        );
        assert!(lock_path.exists(), "lock file must not be unlinked");

        // Full acquire with a timeout below the confirmation delay: gives up
        // (None) instead of breaking the live holder.
        let got = try_lock_auth_file_async(&path, StdDuration::from_millis(300)).await;
        assert!(
            got.is_none(),
            "short-timeout waiter must not break a live-but-stale holder"
        );
        assert!(lock_path.exists(), "lock file must survive the failed wait");

        // Break policy (the post-confirmation Phase 3 path) does break it.
        match try_acquire_once(&lock_path, StuckLivePolicy::Break) {
            LockAttempt::StaleUnlinked => {}
            _ => panic!("Break policy must unlink the confirmed-stuck holder"),
        }
    }

    /// The heartbeat keeps a held lock's holder info fresh: after an interval
    /// elapses the `PID:TS` timestamp is re-dated (fresh wall-clock ts), so a
    /// waiter classifying the holder sees `Alive`, not `StuckLive`. Uses a
    /// short interval — the production cadence only changes how often, not
    /// whether, the rewrite happens.
    #[cfg(unix)]
    #[test]
    fn heartbeat_refreshes_holder_info() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("auth.json.lock");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&lock_path)
            .unwrap();

        // Backdated holder info: what a waiter sees at wake from a long
        // suspend (ts as old as the sleep).
        let old_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 200;
        write!(file, "{}:{old_ts}", std::process::id()).unwrap();
        file.sync_all().unwrap();
        assert_eq!(holder_state(&mut file), HolderState::StuckLive);

        let hb = LockHeartbeat::spawn(file.try_clone().unwrap(), StdDuration::from_millis(20));

        // Within a few intervals the holder must have re-dated itself.
        let deadline = std::time::Instant::now() + StdDuration::from_secs(5);
        loop {
            if holder_state(&mut file) == HolderState::Alive {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "heartbeat never re-dated the holder info"
            );
            std::thread::sleep(StdDuration::from_millis(10));
        }
        drop(hb); // stops + joins the heartbeat thread
    }

    #[cfg(unix)]
    #[test]
    fn test_inodes_match_same_file() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("test.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&lock_path)
            .unwrap();

        assert!(inodes_match(&file, &lock_path).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn test_inodes_mismatch_after_unlink_recreate() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("test.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&lock_path)
            .unwrap();

        std::fs::remove_file(&lock_path).unwrap();
        let _new_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&lock_path)
            .unwrap();

        assert!(!inodes_match(&file, &lock_path).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn test_still_live_detects_broken_lock() {
        // A held guard reports `still_live() == true`; after a sibling breaks
        // the lock (unlink + recreate on a fresh inode, the stale-recovery
        // path) the SAME guard reports `false`. This is what lets a
        // suspended-then-resumed holder notice its lock was reclaimed and
        // refuse to spend the refresh token.
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);

        let lock = try_lock_auth_file_nonblocking(&path).expect("acquire");
        assert!(lock.still_live(&path), "freshly acquired lock must be live");

        // Simulate the stale-recovery break performed by another process.
        let lock_path = path.with_file_name("auth.json.lock");
        std::fs::remove_file(&lock_path).unwrap();
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();

        assert!(
            !lock.still_live(&path),
            "after unlink+recreate the held guard must report not-live"
        );
    }

    // ── Async tests against the production code path ─────────────────

    #[test]
    fn phase2_budget_reserves_confirmation_delay() {
        let confirm = StdDuration::from_secs(12);
        // Refresh-sized budget: Phase 2 gives up the confirmation slice.
        assert_eq!(
            phase2_budget(StdDuration::from_secs(45), confirm),
            StdDuration::from_secs(33)
        );
        // Budget below the delay: Phase 3 never confirms, so Phase 2
        // keeps everything.
        assert_eq!(
            phase2_budget(StdDuration::from_secs(10), confirm),
            StdDuration::from_secs(10)
        );
        // Boundary: equal budget reserves the whole thing for Phase 3.
        assert_eq!(phase2_budget(confirm, confirm), StdDuration::ZERO);
    }

    /// Heartbeat attaches only to refresh-sized holds (the ones that span
    /// an IdP exchange); short advisory holds must not spawn a thread per
    /// acquisition.
    #[tokio::test]
    async fn heartbeat_attached_only_for_refresh_sized_budgets() {
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);

        let short = try_lock_auth_file_async(&path, crate::auth::manager::AUTH_LOCK_TIMEOUT)
            .await
            .expect("uncontended acquire");
        assert!(
            short._heartbeat.is_none(),
            "an AUTH_LOCK_TIMEOUT-sized hold must not carry a heartbeat"
        );
        drop(short);

        let refresh = try_lock_auth_file_async(&path, crate::auth::manager::REFRESH_LOCK_TIMEOUT)
            .await
            .expect("uncontended acquire");
        assert!(
            refresh._heartbeat.is_some(),
            "a REFRESH_LOCK_TIMEOUT-sized hold must carry the heartbeat"
        );
    }

    /// The caller's `timeout` is the TOTAL budget: a live-but-stale holder
    /// forces the full Phase-2 wait + Phase-3 confirmation, and the sum
    /// must still land within the budget (pre-fix: `timeout + confirm`,
    /// ~57 s on a 45 s request).
    #[cfg(unix)]
    #[tokio::test]
    async fn total_wait_stays_within_timeout_budget() {
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);
        let lock_path = path.with_file_name("auth.json.lock");

        // Live-but-stale holder on a separate FD (same-process flock on a
        // different open file description contends like a sibling).
        let mut holder = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&lock_path)
            .unwrap();
        holder.try_lock_exclusive().unwrap();
        let old_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 200;
        write!(holder, "{}:{old_ts}", std::process::id()).unwrap();
        holder.sync_all().unwrap();

        let timeout = StdDuration::from_millis(900);
        let confirm = StdDuration::from_millis(400);
        let start = tokio::time::Instant::now();
        // The holder is our own live PID, so the Break re-observation
        // unlinks and acquires (same shape as the wedged-holder test).
        let lock = try_lock_auth_file_async_with(&path, timeout, confirm).await;
        let elapsed = start.elapsed();
        assert!(lock.is_some(), "wedged holder must be broken");
        // Generous slack for CI scheduling, but well under the pre-fix
        // floor of timeout + confirm (1300 ms).
        assert!(
            elapsed < timeout + StdDuration::from_millis(250),
            "total wait must stay within the caller's budget, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn test_async_acquire_release_basic() {
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);

        let lock = try_lock_auth_file_async(&path, StdDuration::from_secs(1)).await;
        assert!(lock.is_some(), "should acquire lock");

        // Verify lock file has holder info.
        let lock_path = path.with_file_name("auth.json.lock");
        let content = std::fs::read_to_string(&lock_path).unwrap();
        let (pid, _ts) = parse_holder_info(&content).unwrap();
        assert_eq!(pid, std::process::id());

        // Release.
        drop(lock);

        // Re-acquire should succeed.
        let lock2 = try_lock_auth_file_async(&path, StdDuration::from_secs(1)).await;
        assert!(lock2.is_some(), "should re-acquire after release");
    }

    #[tokio::test]
    async fn test_async_contended_lock_times_out() {
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);

        let lock1 = try_lock_auth_file_async(&path, StdDuration::from_secs(1)).await;
        assert!(lock1.is_some());

        // Second acquire should time out (same process, different FD —
        // WouldBlock but holder is alive + recent).
        let lock2 = try_lock_auth_file_async(&path, StdDuration::from_millis(500)).await;
        assert!(lock2.is_none(), "should time out when lock is held");

        drop(lock1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_async_acquire_after_leftover_dead_pid_file() {
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);
        let lock_path = path.with_file_name("auth.json.lock");

        let dead_pid: u32 = i32::MAX as u32;
        std::fs::write(&lock_path, format!("{dead_pid}:9999999999")).unwrap();

        let lock = try_lock_auth_file_async(&path, StdDuration::from_secs(1)).await;
        assert!(lock.is_some(), "should acquire over leftover dead-PID file");

        let content = std::fs::read_to_string(&lock_path).unwrap();
        let (pid, _ts) = parse_holder_info(&content).unwrap();
        assert_eq!(pid, std::process::id());
    }

    // ── Real cross-process integration tests (async) ─────────────────
    //
    // These spawn a genuine second process so we exercise OS-level flock
    // semantics that threads/extra FDs cannot model: a *dead* holder PID
    // (flock auto-released on process death) and `is_process_alive()`
    // recovery. Following the in-repo subprocess-isolation pattern
    // (`xai-crash-handler/tests/integration.rs`), the holder is this very
    // test binary re-executed via `current_exe()`, gated by the
    // `GROK_TEST_LOCK_HOLDER` env var on an `#[ignore]`d entry-point test —
    // no external `python3` dependency.

    /// Line printed to stdout once the subprocess holds the flock.
    #[cfg(unix)]
    const LOCK_HOLDER_READY: &str = "__GROK_LOCK_HOLDER_READY__";

    /// Subprocess entry point for the cross-process lock tests. Only does
    /// anything when re-executed with `GROK_TEST_LOCK_HOLDER` set; a normal
    /// `cargo test` run sees the env var absent and returns immediately
    /// (it is `#[ignore]`d anyway).
    ///
    /// Spec format: `"<lock_path>|<mode>|<age_secs>"`
    ///   - `pid`   → write `PID:TS` holder info, `TS` backdated by `age_secs`
    ///   - `empty` → leave the file empty; backdate its mtime by `age_secs`
    ///
    /// Holds an exclusive flock, prints [`LOCK_HOLDER_READY`], then blocks
    /// on stdin until the parent writes a line, closes the pipe, or kills us.
    #[cfg(unix)]
    #[test]
    #[ignore = "spawned as a subprocess by the cross-process lock tests"]
    fn subprocess_lock_holder() {
        let Ok(spec) = std::env::var("GROK_TEST_LOCK_HOLDER") else {
            return; // normal test run — not a subprocess invocation
        };
        let mut parts = spec.splitn(3, '|');
        let lock_path = parts.next().expect("spec lock_path");
        let mode = parts.next().expect("spec mode");
        let age_secs: u64 = parts.next().expect("spec age").parse().expect("age parse");

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .expect("open lock file");
        file.lock_exclusive().expect("flock");

        match mode {
            "pid" => {
                file.set_len(0).unwrap();
                file.seek(io::SeekFrom::Start(0)).unwrap();
                write!(file, "{}:{}", std::process::id(), now - age_secs).unwrap();
                file.sync_all().unwrap();
            }
            "empty" => {
                file.set_len(0).unwrap();
                file.sync_all().unwrap();
                if age_secs > 0 {
                    let old = filetime::FileTime::from_unix_time((now - age_secs) as i64, 0);
                    filetime::set_file_mtime(lock_path, old).unwrap();
                }
            }
            other => panic!("unknown lock-holder mode: {other:?}"),
        }

        println!("{LOCK_HOLDER_READY}");
        io::stdout().flush().unwrap();

        // Block until released by the parent (line on stdin / closed pipe)
        // or SIGKILL (the OS releases the flock on process death).
        let mut line = String::new();
        let _ = io::stdin().read_line(&mut line);
    }

    /// Re-execute this test binary as a lock-holder subprocess and return
    /// once it signals that it holds the flock. `mode` is `"pid"` or
    /// `"empty"`; `age_secs` backdates the holder timestamp (`pid`) or the
    /// file mtime (`empty`).
    #[cfg(unix)]
    fn spawn_lock_holder_subprocess(
        lock_path: &std::path::Path,
        mode: &str,
        age_secs: u64,
    ) -> std::process::Child {
        use std::io::BufRead;

        let exe = std::env::current_exe().expect("current_exe");
        let spec = format!("{}|{mode}|{age_secs}", lock_path.to_str().unwrap());
        #[allow(clippy::disallowed_methods)] // test fixture; the test kills it
        let mut child = std::process::Command::new(exe)
            .env("GROK_TEST_LOCK_HOLDER", spec)
            .args([
                "--ignored",
                "--exact",
                "--nocapture",
                "auth::manager::lock::tests::subprocess_lock_holder",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn lock-holder subprocess");

        // Read stdout until the ready marker, skipping libtest's
        // `--nocapture` banner lines. Borrow stdout (don't `take`) so the
        // pipe stays open for the child's later libtest output, and scope
        // the borrow so `child` can be moved out on return.
        {
            let stdout = child.stdout.as_mut().expect("child stdout");
            let mut reader = std::io::BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                let n = reader.read_line(&mut line).expect("read child stdout");
                assert!(n > 0, "child exited before signaling ready");
                if line.trim() == LOCK_HOLDER_READY {
                    break;
                }
            }
        }
        child
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_async_real_stale_holder_broken_by_old_timestamp() {
        // Wedged LIVE holder (stale holder info, never heartbeats):
        // breakable, but only via the Phase-3 still-stale re-observation —
        // never on first sight. Short confirm delay injected for speed.
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);
        let lock_path = path.with_file_name("auth.json.lock");

        let mut child = spawn_lock_holder_subprocess(&lock_path, "pid", 120);
        let child_pid = child.id();

        assert!(is_process_alive(child_pid));

        // Budget below the confirmation delay: no break.
        let confirm = StdDuration::from_millis(400);
        let lock =
            try_lock_auth_file_async_with(&path, StdDuration::from_millis(100), confirm).await;
        assert!(
            lock.is_none(),
            "short-budget waiter must not break a live-but-stale holder"
        );

        // Budget at/above the delay: still stale after re-observation → break.
        let lock =
            try_lock_auth_file_async_with(&path, StdDuration::from_millis(500), confirm).await;
        assert!(
            lock.is_some(),
            "wedged live holder must be breakable after the confirmation wait"
        );

        // Verify our PID was written to the NEW lock file.
        let content = std::fs::read_to_string(&lock_path).unwrap();
        let (pid, _) = parse_holder_info(&content).unwrap();
        assert_eq!(pid, std::process::id());

        // Child is still alive (flock on the old unlinked inode).
        assert!(is_process_alive(child_pid));

        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_async_breaks_old_empty_lock_held_by_live_holder() {
        // Regression: a LIVE process holding the flock on an EMPTY lock file
        // (no `PID:TS`) used to be "alive forever", wedging refresh. Old
        // empty locks classify StuckLive (an unidentifiable holder can't be
        // distinguished from one that just woke), so the break goes through
        // the Phase-3 confirmation re-observation.
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);
        let lock_path = path.with_file_name("auth.json.lock");

        let mut child =
            spawn_lock_holder_subprocess(&lock_path, "empty", STALE_LOCK_TIMEOUT_SECS + 30);
        assert!(is_process_alive(child.id()));

        let confirm = StdDuration::from_millis(400);
        let lock =
            try_lock_auth_file_async_with(&path, StdDuration::from_millis(500), confirm).await;
        assert!(lock.is_some(), "should break old empty lock held by child");

        // The fresh lock file must carry our parseable holder info.
        let content = std::fs::read_to_string(&lock_path).unwrap();
        let (pid, _) = parse_holder_info(&content).unwrap();
        assert_eq!(pid, std::process::id());

        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_async_does_not_break_fresh_empty_lock() {
        // Inverse guard: an EMPTY lock with a RECENT mtime (a holder caught
        // in the sub-ms set_len(0)->write window) must NOT be broken.
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);
        let lock_path = path.with_file_name("auth.json.lock");

        let mut child = spawn_lock_holder_subprocess(&lock_path, "empty", 0); // fresh mtime

        let lock = try_lock_auth_file_async(&path, StdDuration::from_millis(800)).await;
        assert!(
            lock.is_none(),
            "must not break a fresh empty lock (holder may be mid-write)"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_async_real_killed_process_recovery() {
        // Child holds flock then gets SIGKILL'd. Flock released on
        // process death. Parent acquires immediately.
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);
        let lock_path = path.with_file_name("auth.json.lock");

        let mut child = spawn_lock_holder_subprocess(&lock_path, "pid", 0);
        let child_pid = child.id();

        // Verify child's PID in the lock file.
        let content_before = std::fs::read_to_string(&lock_path).unwrap();
        let (written_pid, _) = parse_holder_info(&content_before).unwrap();
        assert_eq!(written_pid, child_pid);

        // Kill the child.
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!is_process_alive(child_pid));

        // Lock file still has the dead child's PID.
        let content_after = std::fs::read_to_string(&lock_path).unwrap();
        let (dead_pid, _) = parse_holder_info(&content_after).unwrap();
        assert_eq!(dead_pid, child_pid);

        // Acquire should succeed immediately.
        let start = tokio::time::Instant::now();
        let lock = try_lock_auth_file_async(&path, StdDuration::from_secs(2)).await;
        let elapsed = start.elapsed();

        assert!(lock.is_some(), "should acquire after child killed");
        assert!(
            elapsed < StdDuration::from_secs(1),
            "should be instant, took {elapsed:?}"
        );

        let content = std::fs::read_to_string(&lock_path).unwrap();
        let (pid, _) = parse_holder_info(&content).unwrap();
        assert_eq!(pid, std::process::id());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_async_real_contention_resolved_after_release() {
        // Child holds flock for ~2s then exits. Parent's blocking flock
        // (Phase 2) wakes immediately on release — no poll lag.
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);
        let lock_path = path.with_file_name("auth.json.lock");

        let mut child = spawn_lock_holder_subprocess(&lock_path, "pid", 0);

        // Release child after 2s delay (on a background thread since
        // stdin.write is blocking).
        let mut stdin = child.stdin.take().unwrap();
        let release_handle = std::thread::spawn(move || {
            std::thread::sleep(StdDuration::from_secs(2));
            let _ = stdin.write_all(b"release\n");
        });

        let start = tokio::time::Instant::now();
        let lock = try_lock_auth_file_async(&path, StdDuration::from_secs(10)).await;
        let elapsed = start.elapsed();

        assert!(lock.is_some(), "should acquire after child exits");
        assert!(
            elapsed >= StdDuration::from_millis(1500),
            "should have waited for child, took {elapsed:?}"
        );
        assert!(
            elapsed < StdDuration::from_secs(5),
            "should not overshoot, took {elapsed:?}"
        );

        release_handle.join().unwrap();
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn test_real_is_process_alive_with_spawned_child() {
        #[allow(clippy::disallowed_methods)] // test fixture; the test kills it
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let pid = child.id();

        assert!(is_process_alive(pid), "child should be alive");

        child.kill().unwrap();
        child.wait().unwrap();

        assert!(!is_process_alive(pid), "child should be dead after kill");
    }

    // ── Blocking acquire unit tests ──────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn test_blocking_acquire_uncontended() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("auth.json.lock");

        let file =
            blocking_acquire(&lock_path).expect("uncontended blocking acquire should succeed");
        let content = std::fs::read_to_string(&lock_path).unwrap();
        let (pid, _ts) = parse_holder_info(&content).unwrap();
        assert_eq!(pid, std::process::id());
        drop(file);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_async_blocking_path_wakes_promptly_on_release() {
        // Child holds flock for 1s. Verify the blocking flock (Phase 2)
        // acquires within 500ms of the child releasing — much faster
        // than a 200ms poll loop would guarantee.
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);
        let lock_path = path.with_file_name("auth.json.lock");

        let mut child = spawn_lock_holder_subprocess(&lock_path, "pid", 0);

        // Release child after 1s.
        let mut stdin = child.stdin.take().unwrap();
        let release_handle = std::thread::spawn(move || {
            std::thread::sleep(StdDuration::from_secs(1));
            let _ = stdin.write_all(b"release\n");
        });

        let start = tokio::time::Instant::now();
        let lock = try_lock_auth_file_async(&path, StdDuration::from_secs(10)).await;
        let elapsed = start.elapsed();

        assert!(lock.is_some(), "should acquire via blocking flock");
        // Should acquire very close to 1s (child hold time), not 1s + poll lag.
        assert!(
            elapsed >= StdDuration::from_millis(800),
            "should have waited for child, took {elapsed:?}"
        );
        assert!(
            elapsed < StdDuration::from_millis(2000),
            "blocking flock should wake promptly, took {elapsed:?}"
        );

        release_handle.join().unwrap();
        let _ = child.wait();
    }
}
