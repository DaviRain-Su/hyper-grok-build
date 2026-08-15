//! Persisted "refresh token possibly consumed" sentinel for [`AuthManager`].
//!
//! Failure mode: an OIDC token exchange straddles a system suspend (typically
//! a macOS dark wake that re-enters sleep with no `WillSleep`). The IdP may
//! have consumed the single-use refresh token while the response carrying its
//! rotated successor was lost to the sleep; re-presenting the on-disk RT past
//! the IdP's ~60 s rotation grace trips reuse detection and revokes the whole
//! token family. The in-call guard (`auth.refresh.retry_suppressed_suspend`)
//! is per-call and in-process; sibling processes would still re-present the
//! same RT hours later. This sidecar file next to `auth.json` — written under
//! the `auth.json.lock` discipline (see [`super::lock`]); clears are atomic
//! best-effort removals — is the cross-process, restart-surviving memory.
//!
//! Concurrency: the `auth.json.lock` witness orders writers, but the safety
//! argument is **fingerprint convergence**, not the lock — every record,
//! stamp, and clear compares the sentinel's RT fingerprint (and scope)
//! against the state it acts on, so racing participants converge: a
//! stale-lock clear only removes a record its own comparison proved
//! obsolete, and a concurrent re-record reinstates the same fingerprint.
//! Record and stamp additionally require a [`LiveAuthFileLock`] (liveness
//! re-proved via `AuthFileLock::live`) because they *create* the gate state
//! right after suspends — exactly when flocks die.
//!
//! Write failures are asymmetric by design: a failed retry **stamp** aborts
//! the presentation (see [`AuthManager::stamp_sentinel_election_or_abort`]),
//! while a failed **record** stays best-effort — nothing is about to be
//! presented there (the caller is returning a transient), and after a failed
//! write the only degradation, siblings ungated, equals pre-sentinel
//! behavior with no better option available.
//!
//! Lifecycle: **set** by `apply_refresh_outcome` when a straddled exchange
//! fails ([`SuspectConsumedRt`]); **gates** `refresh_chain` so the suspect RT
//! is not presented until full wake (dark-wake arm bounded by
//! [`DARK_WAKE_DEFER_MAX`]) and then by at most one process machine-wide per
//! [`SENTINEL_RETRY_COOLDOWN`] ([`SentinelRetryElection`]); **cleared** on a
//! persisted fresh credential, a persisted logout, a definitive
//! `invalid_grant`, or lazily once disk rotates past the suspect RT.
//!
//! [`DARK_WAKE_DEFER_MAX`]: super::sleep_gate::DARK_WAKE_DEFER_MAX

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use serde::{Deserialize, Serialize};
use sha2::Digest;
use xai_grok_auth::bearer_suffix;

use super::AuthManager;
use crate::auth::error::{AuthError, TransientReason};
use crate::auth::manager::RefreshReason;
use crate::auth::refresh::{SuspectConsumedRt, resolve_refresh_credential};
use crate::auth::storage::{AuthFileLock, LiveAuthFileLock};
use crate::auth::token_type::TokenType;

/// Minimum wall-clock spacing between coordinated retries of a suspect RT;
/// followers inside the window back off with a transient error. Sized to the
/// assumed IdP rotation grace (`ROTATION_GRACE_MS`).
pub(crate) const SENTINEL_RETRY_COOLDOWN: StdDuration = StdDuration::from_secs(60);

/// On-disk record of a refresh token whose IdP-side fate is unknown.
///
/// Stores a SHA-256 fingerprint, never the RT itself — the sidecar must not
/// become a second copy of the credential.
///
/// **Scope-tagged**: `auth.json` is multi-scope (`issuer::client_id`), and a
/// manager on a different scope must neither be gated by nor allowed to clear
/// another scope's sentinel — its own RT never matches, so a scope-blind
/// clear would drop the record the other scope still needs. One slot, last
/// writer wins: concurrent straddles on two scopes of one home are not worth
/// a per-scope map (the losing scope degrades to pre-sentinel behavior).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ConsumedRtSentinel {
    /// SHA-256 (lowercase hex) of the possibly-consumed refresh token.
    rt_sha256: String,
    /// `bearer_suffix` of the RT, for correlation with `auth.refresh.*` logs.
    rt_suffix: String,
    /// Scope whose credential straddled. `None` only on files written by
    /// pre-scope builds (never shipped): matches any scope, self-heals on
    /// the next write.
    #[serde(default)]
    scope: Option<String>,
    /// Unix seconds when the straddled failure was observed. Wall, not
    /// monotonic: the file is shared across processes and the intervals of
    /// interest span suspends, where monotonic clocks pause.
    recorded_at: i64,
    /// What failed (refresher-side message), for forensics.
    reason: String,
    /// Suspended ms the exchange-scoped suspend probe measured.
    suspended_ms: u64,
    /// PID that recorded the sentinel.
    pid: u32,
    /// Unix seconds of the last coordinated retry; `None` until a retrier is
    /// elected.
    #[serde(default)]
    last_retry_at: Option<i64>,
    /// Coordinated retries attempted so far.
    #[serde(default)]
    retry_count: u32,
}

impl ConsumedRtSentinel {
    fn new(suspect: &SuspectConsumedRt, reason: &str, scope: &str) -> Self {
        Self {
            rt_sha256: rt_fingerprint(suspect.refresh_token()),
            rt_suffix: bearer_suffix(suspect.refresh_token()).to_owned(),
            scope: Some(scope.to_owned()),
            recorded_at: unix_now(),
            reason: reason.to_owned(),
            suspended_ms: suspect.suspended_ms(),
            pid: std::process::id(),
            last_retry_at: None,
            retry_count: 0,
        }
    }

    fn matches(&self, rt: &str) -> bool {
        self.rt_sha256 == rt_fingerprint(rt)
    }

    /// Scope-less files (pre-scope builds) match any scope — see struct doc.
    fn matches_scope(&self, scope: &str) -> bool {
        self.scope.as_deref().is_none_or(|s| s == scope)
    }

    /// Seconds since the last coordinated retry; `None` when never stamped
    /// or when the stamp reads as in the future (wall clock stepped back):
    /// honoring a future stamp would hold the machine-wide cooldown closed
    /// until the clock catches up — unbounded, where the cooldown promises
    /// at most [`SENTINEL_RETRY_COOLDOWN`]. Electing instead is safe: the
    /// file lock serializes presentations, so this can only race a retry
    /// that already completed.
    fn secs_since_last_retry(&self) -> Option<u64> {
        self.last_retry_at.and_then(secs_since)
    }

    /// Telemetry only; `None` under the same future-reading convention.
    fn age_secs(&self) -> Option<u64> {
        secs_since(self.recorded_at)
    }

    /// Backdate the retry stamp so cooldown expiry is testable without waits.
    #[cfg(test)]
    pub(crate) fn backdate_last_retry_for_test(&mut self, by: StdDuration) {
        if let Some(last) = self.last_retry_at.as_mut() {
            *last -= by.as_secs() as i64;
        }
    }

    /// Stamp a retry `ahead` in the future, as a wall-clock step-back leaves.
    #[cfg(test)]
    pub(crate) fn stamp_future_retry_for_test(&mut self, ahead: StdDuration) {
        self.last_retry_at = Some(unix_now() + ahead.as_secs() as i64);
        self.retry_count += 1;
    }

    #[cfg(test)]
    pub(crate) fn retry_count_for_test(&self) -> u32 {
        self.retry_count
    }

    /// Whether a coordinated retry has been stamped (cooldown consumed).
    #[cfg(test)]
    pub(crate) fn has_stamped_retry_for_test(&self) -> bool {
        self.last_retry_at.is_some()
    }

    #[cfg(test)]
    pub(crate) fn matches_for_test(&self, rt: &str) -> bool {
        self.matches(rt)
    }
}

/// A coordinated-retry election won in
/// [`AuthManager::check_consumed_sentinel_gate`] but **not yet stamped** into
/// the on-disk sentinel. Two invariants hang on this split:
///
/// 1. Stamp happens-before presentation: the winner stamps
///    ([`AuthManager::stamp_sentinel_election`]) strictly before the IdP
///    call, so a crash mid-exchange still counts against the cooldown.
/// 2. Deterministic pre-IdP aborts don't consume the election: a dropped,
///    unstamped election leaves the sentinel untouched, so a sibling can be
///    elected the moment conditions clear.
///
/// Race-free because the `auth.json` file lock is held from election through
/// stamp to exchange, with no await in between. `Debug` is safe: only the
/// RT's fingerprint and telemetry suffix, never the token.
#[derive(Debug)]
pub(crate) struct SentinelRetryElection {
    sentinel: ConsumedRtSentinel,
}

fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Seconds since `unix_secs`; `None` when it reads as in the future (a
/// stepped-back wall clock) — one convention for every interval this file
/// derives from its cross-process wall-clock stamps.
fn secs_since(unix_secs: i64) -> Option<u64> {
    u64::try_from(unix_now() - unix_secs).ok()
}

/// SHA-256 fingerprint (lowercase hex) of a refresh token. `pub(super)` so
/// manager tests can hand-build legacy sentinel files with real fingerprints.
pub(super) fn rt_fingerprint(rt: &str) -> String {
    sha2::Sha256::digest(rt.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Sidecar path, a fixed sibling of `auth.json` (like `auth.json.lock`).
fn sentinel_path(auth_json_path: &Path) -> PathBuf {
    auth_json_path.with_file_name("auth.json.rt-sentinel")
}

/// Test-only, path-scoped write fault (mirrors `storage::WRITE_FAULT_PATH`):
/// `write_sentinel_file` fails with `Unsupported` for exactly this path.
#[cfg(test)]
pub(crate) static SENTINEL_WRITE_FAULT_PATH: std::sync::Mutex<Option<PathBuf>> =
    std::sync::Mutex::new(None);

/// Test-only, path-scoped fault for the final tmp → sentinel rename inside
/// [`replace_via_backup`], so the restore contract is exercisable.
#[cfg(test)]
pub(crate) static SENTINEL_RENAME_FAULT_PATH: std::sync::Mutex<Option<PathBuf>> =
    std::sync::Mutex::new(None);

/// Test-only: whether the park (`.bak`) existed when the injected rename
/// fault fired — proves the park-before-rename ordering the restore
/// contract depends on.
#[cfg(test)]
pub(crate) static SENTINEL_RENAME_FAULT_SAW_BAK: std::sync::Mutex<Option<bool>> =
    std::sync::Mutex::new(None);

/// Park path for [`replace_via_backup`], as an explicit file-name append:
/// `with_extension` on the multi-dot sidecar name strips `.rt-sentinel` and
/// would name the bak as if it belonged to `auth.json` itself — a collision
/// with any future `auth.json.*` sibling using the same scheme. Shared with
/// the restore-contract test so the two cannot diverge.
pub(super) fn sentinel_bak_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".bak.{}", std::process::id()));
    path.with_file_name(name)
}

/// Windows-shaped safe replace — `rename` cannot reliably replace an
/// existing target there, and a bare pre-remove would leave the gate empty
/// for the next pass when the rename then fails. Invariant (pinned by
/// `failed_replace_keeps_previous_sentinel_readable`): **a failed replace
/// leaves the previous sentinel content readable.** Park the existing
/// sidecar as `.bak`, rename tmp into place, restore `.bak` on failure;
/// only if the restore also fails, write `json` directly to the final path
/// (non-atomic, under the held flock — a torn read parses as absent, no
/// worse than gone). Platform-independent so POSIX CI can pin the contract;
/// the `cfg(windows)` selection above cannot run there.
#[cfg_attr(not(windows), allow(dead_code))]
fn final_rename(tmp: &Path, path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if SENTINEL_RENAME_FAULT_PATH
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_deref()
        == Some(path)
    {
        *SENTINEL_RENAME_FAULT_SAW_BAK
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(sentinel_bak_path(path).exists());
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "injected rename fault (SENTINEL_RENAME_FAULT_PATH)",
        ));
    }
    std::fs::rename(tmp, path)
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn replace_via_backup(tmp: &Path, path: &Path, json: &str) -> std::io::Result<()> {
    let bak = sentinel_bak_path(path);
    let had_existing = match std::fs::rename(path, &bak) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        // Can't park the existing sidecar: leave it (and the caller's tmp
        // reclaim) as-is — the previous content stays readable.
        Err(e) => return Err(e),
    };
    match final_rename(tmp, path) {
        Ok(()) => {
            if had_existing {
                let _ = std::fs::remove_file(&bak);
            }
            Ok(())
        }
        Err(e) => {
            if had_existing && std::fs::rename(&bak, path).is_err() {
                // Restore failed too: last resort, the new content lands
                // non-atomically so the gate is not left empty.
                if std::fs::write(path, json).is_ok() {
                    let _ = crate::util::secure_file::ensure_owner_only_permissions(path);
                    let _ = std::fs::remove_file(&bak);
                    return Ok(());
                }
            }
            Err(e)
        }
    }
}

/// Atomic (temp + rename) owner-only write, mirroring `write_auth_json`'s
/// crash-safety: a torn sentinel would read as absent and silently drop the
/// only cross-process record of the straddle.
fn write_sentinel_file(path: &Path, sentinel: &ConsumedRtSentinel) -> std::io::Result<()> {
    #[cfg(test)]
    if SENTINEL_WRITE_FAULT_PATH
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_deref()
        == Some(path)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "injected write fault (SENTINEL_WRITE_FAULT_PATH)",
        ));
    }
    let json = serde_json::to_string_pretty(sentinel)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let result = (|| {
        let mut file = crate::util::secure_file::open_secure_file(&tmp)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        // mode(0o600) applies only on create; a leftover tmp is truncated,
        // not re-created, so re-assert like `write_store_to` does.
        crate::util::secure_file::ensure_owner_only_permissions(&tmp)?;
        // Windows can't reliably rename over an existing target; a bare
        // pre-remove would wipe the gate for the *next* pass if the rename
        // then fails, so it goes through the backup dance instead. POSIX
        // keeps the plain atomic rename(2). Concurrent writers are excluded
        // by the `auth.json.lock` held at both write sites.
        #[cfg(windows)]
        {
            replace_via_backup(&tmp, path, &json)
        }
        #[cfg(not(windows))]
        {
            std::fs::rename(&tmp, path)
        }
    })();
    match result {
        Ok(()) => {
            // Best-effort belt-and-braces; the rename already published 0600.
            if let Err(e) = crate::util::secure_file::ensure_owner_only_permissions(path) {
                tracing::warn!(error = %e, path = %path.display(), "auth: failed to re-assert rt-sentinel permissions");
            }
            Ok(())
        }
        Err(e) => {
            // Reclaim the tmp so failed writes don't accumulate orphans.
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

impl AuthManager {
    fn consumed_sentinel_path(&self) -> PathBuf {
        sentinel_path(self.auth_json_path())
    }

    /// Read the sentinel, treating a missing or unparseable file as absent.
    /// Unparseable is absent (not an error): losing the sentinel degrades to
    /// pre-sentinel behavior, and a torn write must not wedge refresh.
    pub(crate) fn read_consumed_sentinel(&self) -> Option<ConsumedRtSentinel> {
        let path = self.consumed_sentinel_path();
        let bytes = std::fs::read(&path).ok()?;
        match serde_json::from_slice(&bytes) {
            Ok(sentinel) => Some(sentinel),
            Err(e) => {
                tracing::debug!(error = %e, path = %path.display(), "auth: unparseable rt-sentinel, treating as absent");
                None
            }
        }
    }

    /// Persist the sentinel, tagged with this manager's scope, under a
    /// **live** `auth.json` file lock (`_lock` type-enforces it — recording
    /// happens right after a suspend, exactly when flocks die). Retry
    /// bookkeeping of an existing same-RT sentinel is carried forward so a
    /// repeat straddle cannot reset the coordinated retry cooldown.
    pub(crate) fn record_consumed_sentinel(
        &self,
        suspect: &SuspectConsumedRt,
        reason: &str,
        _lock: &LiveAuthFileLock<'_>,
    ) {
        let mut sentinel = ConsumedRtSentinel::new(suspect, reason, &self.scope);
        if let Some(existing) = self.read_consumed_sentinel()
            && existing.matches(suspect.refresh_token())
            && existing.matches_scope(&self.scope)
        {
            sentinel.last_retry_at = existing.last_retry_at;
            sentinel.retry_count = existing.retry_count;
        }
        let path = self.consumed_sentinel_path();
        match write_sentinel_file(&path, &sentinel) {
            Ok(()) => {
                xai_grok_telemetry::unified_log::warn(
                    "auth.refresh.consumed_sentinel_set",
                    None,
                    Some(serde_json::json!({
                        "rt_suffix": sentinel.rt_suffix,
                        "scope": &self.scope,
                        "reason": reason,
                        "suspended_ms": sentinel.suspended_ms,
                        "retry_count": sentinel.retry_count,
                        "path": path.display().to_string(),
                    })),
                );
            }
            Err(e) => {
                // Degrades to pre-sentinel behavior, but must be attributable
                // in a post-incident capture.
                xai_grok_telemetry::unified_log::error(
                    "auth.refresh.consumed_sentinel_write_failed",
                    None,
                    Some(serde_json::json!({
                        "error": e.to_string(),
                        "path": path.display().to_string(),
                    })),
                );
            }
        }
        // A new episode gets a fresh DARK_WAKE_DEFER_MAX budget; a leftover
        // run would shorten (or instantly exhaust) this one's bound.
        self.sentinel_dark_wake_defer_since.reset();
    }

    /// Remove the sentinel (idempotent, best-effort), under the `auth.json`
    /// file lock — but only when it belongs to this manager's scope: another
    /// scope's lifecycle events say nothing about whether *our* suspect RT
    /// was consumed (see the [`ConsumedRtSentinel`] scope-tag doc).
    pub(crate) fn clear_consumed_sentinel(&self, reason: &str, _lock: &AuthFileLock) {
        let path = self.consumed_sentinel_path();
        let existing = self.read_consumed_sentinel();
        if let Some(ref sentinel) = existing
            && !sentinel.matches_scope(&self.scope)
        {
            return;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                xai_grok_telemetry::unified_log::info(
                    "auth.refresh.consumed_sentinel_cleared",
                    None,
                    Some(serde_json::json!({
                        "reason": reason,
                        "rt_suffix": existing.as_ref().map(|s| s.rt_suffix.clone()),
                        "age_secs": existing.as_ref().and_then(ConsumedRtSentinel::age_secs),
                    })),
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "auth: failed to clear rt-sentinel");
            }
        }
    }

    /// Credential-persisted clear: remove the sentinel only when `persisted`
    /// carries an RT that supersedes the fingerprinted suspect. A re-save of
    /// the *same* credential (e.g. a privacy-flag toggle through
    /// `save_without_enrichment`) proves nothing about the suspect RT and
    /// must keep the machine-wide gate.
    pub(crate) fn clear_consumed_sentinel_if_superseded(
        &self,
        persisted: &crate::auth::GrokAuth,
        lock: &AuthFileLock,
    ) {
        let Some(sentinel) = self.read_consumed_sentinel() else {
            return;
        };
        if !sentinel.matches_scope(&self.scope) {
            return;
        }
        if persisted
            .refresh_token
            .as_deref()
            .is_some_and(|rt| !sentinel.matches(rt))
        {
            self.clear_consumed_sentinel("credential_persisted", lock);
        }
    }

    /// Whether the sentinel gate should keep deferring for dark wake —
    /// **bounded** by [`DARK_WAKE_DEFER_MAX`] on both [`DualClock`] arms so a
    /// machine stuck reporting a *continuous* dark wake (e.g. no display)
    /// cannot be deferred into the forced logout the sentinel exists to
    /// prevent: past the bound one retry is let through (still subject to
    /// the election + [`SENTINEL_RETRY_COOLDOWN`]) and the run resets.
    ///
    /// Deliberately mirrors [`AuthManager::should_defer_for_dark_wake`] on
    /// its **own** run state: the general budget is reset by
    /// `check_refresh_deferral`'s not-deferring path on every dead-token
    /// pass, so sharing it would restart this budget each attempt and it
    /// could never exhaust.
    fn should_defer_sentinel_for_dark_wake(&self) -> bool {
        let dark = self.is_dark_wake();
        self.sentinel_dark_wake_defer_since
            .should_defer(dark, "auth.refresh.sentinel_defer_budget_exhausted")
    }

    /// Gate an imminent IdP presentation on the sentinel. Called by
    /// `refresh_chain` with the `auth.json` file lock held and proven live
    /// (`lock` type-enforces that; the election is only race-free under it).
    ///
    /// `Ok(None)` when no sentinel applies. `Err` (transient) while
    /// sleep-gated, dark-wake within the bounded budget
    /// ([`Self::should_defer_sentinel_for_dark_wake`]), or within
    /// [`SENTINEL_RETRY_COOLDOWN`] of another process's stamped retry.
    /// Otherwise the caller is the elected retrier and gets an **unstamped**
    /// [`SentinelRetryElection`] — see its doc for why the stamp is deferred
    /// to just before the IdP call.
    pub(crate) fn check_consumed_sentinel_gate(
        &self,
        token_type: TokenType,
        reason: RefreshReason,
        lock: &AuthFileLock,
    ) -> Result<Option<SentinelRetryElection>, AuthError> {
        // Only the OIDC flow presents a single-use RT whose reuse is fatal;
        // the external-binary flow re-runs an operator command.
        if token_type != TokenType::OidcSession {
            return Ok(None);
        }
        let Some(sentinel) = self.read_consumed_sentinel() else {
            return Ok(None);
        };
        if !sentinel.matches_scope(&self.scope) {
            // Another scope's sentinel: treat as absent, and deliberately do
            // NOT clear — the rotated-past clear below would drop the record
            // that scope's processes still gate on.
            return Ok(None);
        }
        let candidate_rt = resolve_refresh_credential(self, self.read_disk_auth_silent(), reason)
            .and_then(|a| a.refresh_token);
        let Some(rt) = candidate_rt else {
            return Ok(None);
        };
        if !sentinel.matches(&rt) {
            // Disk moved past the suspect RT; the obsolete sentinel must not
            // linger to block a future credential that fails differently.
            self.clear_consumed_sentinel("rt_rotated_past_sentinel", lock);
            return Ok(None);
        }
        let sleep_gated = self.is_sleep_gated();
        // Only consulted when not sleep-gated, so a suspend transition does
        // not consume the dark-wake budget (its wall arm ages through sleep).
        let dark_wake_deferral = !sleep_gated && self.should_defer_sentinel_for_dark_wake();
        if sleep_gated || dark_wake_deferral {
            xai_grok_telemetry::unified_log::warn(
                "auth.refresh.sentinel_deferred",
                None,
                Some(serde_json::json!({
                    "rt_suffix": sentinel.rt_suffix,
                    "reason": format!("{reason:?}"),
                    "sleep_gated": sleep_gated,
                    "dark_wake": dark_wake_deferral,
                    "age_secs": sentinel.age_secs(),
                    "transient_reason": TransientReason::SentinelAwaitingWake.as_str(),
                })),
            );
            return Err(AuthError::transient_reason(
                TransientReason::SentinelAwaitingWake,
                "refresh deferred: refresh token possibly consumed by an exchange \
                 that straddled a suspend; retrying only at full wake",
            ));
        }
        if let Some(since) = sentinel.secs_since_last_retry()
            && since < SENTINEL_RETRY_COOLDOWN.as_secs()
        {
            xai_grok_telemetry::unified_log::warn(
                "auth.refresh.sentinel_retry_backoff",
                None,
                Some(serde_json::json!({
                    "rt_suffix": sentinel.rt_suffix,
                    "since_last_retry_secs": since,
                    "cooldown_secs": SENTINEL_RETRY_COOLDOWN.as_secs(),
                    "retry_count": sentinel.retry_count,
                    "transient_reason": TransientReason::SentinelCooldown.as_str(),
                })),
            );
            return Err(AuthError::transient_reason(
                TransientReason::SentinelCooldown,
                "refresh deferred: a sibling process was already elected to retry \
                 the possibly-consumed refresh token",
            ));
        }
        Ok(Some(SentinelRetryElection { sentinel }))
    }

    /// Stamp a won [`SentinelRetryElection`] into the on-disk sentinel,
    /// starting the [`SENTINEL_RETRY_COOLDOWN`] for every other participant.
    /// Must run under the same file lock the election was won under (`_lock`
    /// type-enforces that), strictly before the IdP call — see
    /// [`SentinelRetryElection`] for the invariants.
    /// Stamp the won election, or **abort the presentation**:
    ///
    /// - flock died in the election-to-stamp window — a suspend there lets a
    ///   sibling break the lock, elect, and present the same RT;
    /// - stamp write failed — election exclusivity depends on the durable
    ///   stamp (once the lock drops a sibling elects immediately), and a
    ///   process that cannot write the sidecar likely cannot persist a
    ///   rotated credential either (same directory, same write path):
    ///   presenting an RT whose successor then fails to persist is the
    ///   lost-successor family-kill itself. Can't-write ⇒ shouldn't-present.
    ///   No wedge: if the disk never recovers, the refresh outcome was
    ///   unpersistable anyway; if it recovers, the next pass's gate retries
    ///   cleanly.
    ///
    /// No re-acquire or write retry here: the next refresh pass re-runs the
    /// full gate under a fresh lock, election included — the safe retry.
    pub(crate) fn stamp_sentinel_election_or_abort(
        &self,
        election: SentinelRetryElection,
        reason: RefreshReason,
        file_lock: &AuthFileLock,
    ) -> Result<(), AuthError> {
        let Some(live) = file_lock.live(self.auth_json_path()) else {
            xai_grok_telemetry::unified_log::warn(
                "auth.refresh.sentinel_stamp_skipped_lock_lost",
                None,
                Some(serde_json::json!({
                    "reason": format!("{reason:?}"),
                    "transient_reason": TransientReason::SentinelLockLost.as_str(),
                })),
            );
            return Err(AuthError::transient_reason(
                TransientReason::SentinelLockLost,
                "refresh aborted: auth.json.lock lost before the sentinel \
                 retry stamp; retrying re-runs the election under a fresh lock",
            ));
        };
        self.stamp_sentinel_election(election, reason, &live)
    }

    pub(crate) fn stamp_sentinel_election(
        &self,
        election: SentinelRetryElection,
        reason: RefreshReason,
        _lock: &LiveAuthFileLock<'_>,
    ) -> Result<(), AuthError> {
        let mut sentinel = election.sentinel;
        sentinel.last_retry_at = Some(unix_now());
        sentinel.retry_count += 1;
        if let Err(e) = write_sentinel_file(&self.consumed_sentinel_path(), &sentinel) {
            xai_grok_telemetry::unified_log::warn(
                "auth.refresh.sentinel_stamp_write_failed",
                None,
                Some(serde_json::json!({
                    "error": e.to_string(),
                    "reason": format!("{reason:?}"),
                    "transient_reason": TransientReason::SentinelStampFailed.as_str(),
                })),
            );
            return Err(AuthError::transient_reason(
                TransientReason::SentinelStampFailed,
                "refresh aborted: could not stamp the sentinel retry; \
                 a process that cannot write the stamp must not present the \
                 suspect refresh token",
            ));
        }
        xai_grok_telemetry::unified_log::warn(
            "auth.refresh.sentinel_retry",
            None,
            Some(serde_json::json!({
                "rt_suffix": sentinel.rt_suffix,
                "retry_count": sentinel.retry_count,
                "age_secs": sentinel.age_secs(),
                "reason": format!("{reason:?}"),
                // Marks a retry forced through an exhausted dark-wake budget.
                "dark_wake": self.is_dark_wake(),
            })),
        );
        Ok(())
    }

    /// Sentinel injection with full field control (e.g. a stamped retry).
    #[cfg(test)]
    pub(crate) fn write_consumed_sentinel_for_test(&self, sentinel: &ConsumedRtSentinel) {
        write_sentinel_file(&self.consumed_sentinel_path(), sentinel)
            .expect("test sentinel write must succeed");
    }

    /// Constructor mirror so tests build sentinels through production
    /// fingerprinting and scope-tagging.
    #[cfg(test)]
    pub(crate) fn make_sentinel_for_test(
        &self,
        suspect: &SuspectConsumedRt,
        reason: &str,
    ) -> ConsumedRtSentinel {
        ConsumedRtSentinel::new(suspect, reason, &self.scope)
    }
}
