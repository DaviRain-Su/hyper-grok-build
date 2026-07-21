use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;

use super::model::{
    API_KEY_SCOPE, AuthMode, AuthStore, GrokAuth, KIMI_CODE_OAUTH_SCOPE, OPENAI_CODEX_OAUTH_SCOPE,
    lookup_auth, platform_api_key_scope,
};

/// RAII guard for an exclusive advisory lock on `auth.json.lock`.
/// The lock is released when the inner `File` is dropped (closing the FD).
pub(crate) struct AuthFileLock {
    pub(super) _file: File,
}

impl AuthFileLock {
    /// Returns `true` while this guard still refers to the **live**
    /// `auth.json.lock` inode.
    ///
    /// A waiter that finds a holder stuck past the stale-lock timeout breaks
    /// the lock by `unlink`ing the file and recreating it on a fresh inode
    /// (see [`crate::auth::manager::lock`]). The usual cause of a "stuck"
    /// holder is a process **suspended across system sleep** while holding the
    /// lock: it stays alive (so the kernel never releases its flock) yet makes
    /// no progress, so siblings break it. When such a holder resumes, its
    /// flock lives on the now-deleted inode — it no longer holds the live lock
    /// even though this `AuthFileLock` still exists.
    ///
    /// Callers about to perform an irreversible, lock-protected action
    /// (sending a refresh token to the IdP, writing `auth.json`) MUST
    /// re-validate first; otherwise two processes can spend the same refresh
    /// token and trip token-family revocation.
    ///
    /// Non-Unix has no inode concept, so this conservatively returns `true`.
    #[cfg(unix)]
    pub(crate) fn still_live(&self, auth_json_path: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        let lock_path = auth_json_path.with_file_name("auth.json.lock");
        let (Ok(fd_meta), Ok(path_meta)) = (self._file.metadata(), std::fs::metadata(&lock_path))
        else {
            // Lock file gone or unreadable → we no longer hold the live lock.
            return false;
        };
        fd_meta.ino() == path_meta.ino() && fd_meta.dev() == path_meta.dev()
    }

    #[cfg(not(unix))]
    pub(crate) fn still_live(&self, _auth_json_path: &Path) -> bool {
        true
    }
}

/// Resolve the path to the user's `auth.json`.
///
/// Honors `GROK_AUTH_PATH` so tests can point at a scratch file instead of a
/// developer's real `~/.grok/auth.json`. Falls back to `$GROK_HOME/auth.json`.
pub fn auth_json_path() -> PathBuf {
    std::env::var("GROK_AUTH_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| xai_grok_config::grok_home().join("auth.json"))
}

/// Resolve the auth.json path for a storage helper that still takes `grok_home`.
///
/// When `GROK_AUTH_PATH` is set, use that exact path (including a non-default
/// basename such as `scratch.json`). Otherwise fall back to
/// `grok_home.join("auth.json")` so hermetic tests that pass a tempdir keep
/// working without setting the env var.
fn resolve_auth_json_path(grok_home: &Path) -> PathBuf {
    if std::env::var_os("GROK_AUTH_PATH").is_some() {
        auth_json_path()
    } else {
        grok_home.join("auth.json")
    }
}

pub fn read_auth_json(auth_file: &Path) -> std::io::Result<AuthStore> {
    let mut file = File::open(auth_file)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;

    // Tighten world-readable copies (hand-restored, umask edge cases, etc.).
    // Best-effort: a chmod failure must not block login/read paths.
    if let Err(e) = crate::util::secure_file::ensure_owner_only_permissions(auth_file) {
        tracing::warn!(
            path = %auth_file.display(),
            error = %e,
            "auth: failed to enforce owner-only permissions on auth.json"
        );
    }

    // Empty files are valid (recover from prior crash/partial write).
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        return Ok(AuthStore::new());
    }

    let map = serde_json::from_str(trimmed)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(map)
}

/// Read auth.json, returning an empty map if the file does not exist.
///
/// Non-empty corrupt JSON, permission errors, etc. are returned as errors
/// so the caller can decide whether to skip the write (to avoid clobbering
/// sibling scopes).
///
/// Kept for the test-only `persist_and_swap` and as a strict reader.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "used from tests only; remove expect when wired in production"
    )
)]
pub(crate) fn read_auth_json_or_empty(auth_file: &Path) -> std::io::Result<AuthStore> {
    match read_auth_json(auth_file) {
        Ok(map) => Ok(map),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AuthStore::new()),
        Err(e) => Err(e),
    }
}

/// Best-effort backup of a corrupt (unparseable) auth.json.
///
/// If the file exists and `read_auth_json` fails with `InvalidData`,
/// it is renamed to `auth.json.corrupt.<millis>` (sibling in the same
/// directory) and the backup path is returned. Used before recovery
/// writes so the original bytes are never silently lost.
pub(crate) fn backup_corrupt_auth_file(path: &Path) -> Option<PathBuf> {
    if !path.exists() {
        return None;
    }
    if read_auth_json(path).is_ok() {
        return None;
    }

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "auth.json".to_string());

    let backup_name = format!("{}.corrupt.{}", file_name, ts);
    let backup = path.with_file_name(backup_name);

    match std::fs::rename(path, &backup) {
        Ok(()) => {
            // Corrupt backups still hold token material — keep them owner-only.
            let _ = crate::util::secure_file::ensure_owner_only_permissions(&backup);
            tracing::warn!(
                original = %path.display(),
                backup = %backup.display(),
                "auth: backed up corrupt auth.json before recovery write"
            );
            // Must reach unified.jsonl: the tracing line above is invisible
            // in production captures, and this is the only record of both
            // the corruption and where the original bytes went.
            xai_grok_telemetry::unified_log::error(
                "auth: corrupt auth.json backed up",
                None,
                Some(serde_json::json!({
                    "original": path.display().to_string(),
                    "backup": backup.display().to_string(),
                })),
            );
            Some(backup)
        }
        Err(e) => {
            tracing::warn!(error = %e, "auth: failed to rename corrupt auth.json for backup");
            xai_grok_telemetry::unified_log::error(
                "auth: corrupt auth.json backup failed",
                None,
                Some(serde_json::json!({
                    "original": path.display().to_string(),
                    "error": e.to_string(),
                })),
            );
            None
        }
    }
}

/// Read auth.json for an upcoming write, with recovery for corrupt files.
///
/// - Missing/empty → empty map (safe to write fresh)
/// - Valid JSON → parsed map
/// - Non-empty corrupt JSON → backs up to `auth.json.corrupt.<millis>`,
///   then returns empty map so the caller can write the new credential.
///
/// Other I/O errors (PermissionDenied, etc.) are still returned as errors.
pub(crate) fn read_auth_json_or_empty_recovering_corrupt(
    auth_file: &Path,
) -> std::io::Result<AuthStore> {
    match read_auth_json(auth_file) {
        Ok(map) => Ok(map),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AuthStore::new()),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            let _ = backup_corrupt_auth_file(auth_file);
            Ok(AuthStore::new())
        }
        Err(e) => Err(e),
    }
}

/// Persist `auth.json`, preferring a crash-safe atomic write but falling
/// back to a non-atomic in-place write when the disk is full.
///
/// The atomic path (temp + rename) needs free space >= the file size,
/// because the old file and a full temp copy coexist until the rename. On a
/// nearly-full disk that temp copy can fail with `StorageFull` (ENOSPC)
/// even though the credentials themselves are tiny. When that happens we
/// retry with an in-place truncate+write, which only needs the freed blocks
/// of the old file — far less than the temp-copy approach.
///
/// The in-place path is non-atomic, with two accepted trade-offs:
/// - If the in-place write itself fails (e.g. a concurrent process grabs the
///   just-freed blocks, or a crash mid-write), the prior bytes are restored
///   best-effort so a torn/empty file never *replaces* the previous on-disk
///   credential — on-disk state ends up no worse than before the attempt.
/// - Unlocked concurrent readers can still observe a torn (partial) file
///   during the brief write window; a partial file is healed on the next
///   read via [`read_auth_json_or_empty_recovering_corrupt`] (backup +
///   relogin). This window is inherent to any sub-1×-free single-file
///   replace and is preferable to persisting nothing at all, which would
///   leave every concurrent process with a stale, already-revoked token.
pub(super) fn write_auth_json(auth_file: &Path, auth_store: &AuthStore) -> std::io::Result<()> {
    write_auth_json_with(auth_file, auth_store, write_auth_json_atomic)
}

/// Dispatch helper: run `atomic`, and on `StorageFull` fall back to an
/// in-place write. Split out (with `atomic` injectable) so the disk-full
/// fallback is unit-testable without an actually-full filesystem.
fn write_auth_json_with(
    auth_file: &Path,
    auth_store: &AuthStore,
    atomic: fn(&Path, &AuthStore) -> std::io::Result<()>,
) -> std::io::Result<()> {
    match atomic(auth_file, auth_store) {
        Err(e) if e.kind() == std::io::ErrorKind::StorageFull => {
            tracing::warn!(
                path = %auth_file.display(),
                "auth: disk full during atomic write, falling back to in-place write"
            );
            // Must reach unified.jsonl: a silent in-memory-only credential
            // (the prior behavior) leaves sibling processes with a stale
            // refresh token and no record of why. Surface it loudly.
            xai_grok_telemetry::unified_log::warn(
                "auth: disk full, falling back to non-atomic in-place write",
                None,
                Some(serde_json::json!({
                    "path": auth_file.display().to_string(),
                })),
            );
            write_auth_json_in_place(auth_file, auth_store)
        }
        other => other,
    }
}

/// Serialize `auth_store` to `path` (truncate + rewrite), owner-only (0o600)
/// and `fsync`'d. Shared core of the atomic path (which targets the temp
/// file) and the in-place fallback (which targets `auth.json` directly).
///
/// Uses streaming `to_writer_pretty` through a `BufWriter` to avoid
/// allocating the entire JSON string in memory — eliminates OOM risk under
/// severe memory pressure.
fn write_store_to(path: &Path, auth_store: &AuthStore) -> std::io::Result<()> {
    use crate::util::secure_file::open_secure_file;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = open_secure_file(path)?;
    let mut writer = std::io::BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, auth_store)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    writer.flush()?;
    writer
        .into_inner()
        .map_err(|e| e.into_error())?
        .sync_all()?;
    // `open_secure_file` mode bits apply only on create; tighten existing paths.
    // Best-effort after durable content: a chmod-only failure must not look
    // like a failed write. The in-place fallback restores the prior snapshot
    // on any `write_store_to` Err, which would discard freshly written tokens.
    // Load path re-tightens on next read.
    if let Err(e) = crate::util::secure_file::ensure_owner_only_permissions(path) {
        tracing::warn!(
            error = %e,
            path = %path.display(),
            "auth: failed to ensure owner-only permissions after write"
        );
    }
    Ok(())
}

/// Atomic write: tmp + rename. Unix `rename(2)` replaces atomically;
/// Windows `rename` requires removing the target first.
fn write_auth_json_atomic(auth_file: &Path, auth_store: &AuthStore) -> std::io::Result<()> {
    let tmp = auth_file.with_extension(format!("json.{}.tmp", std::process::id()));
    write_store_to(&tmp, auth_store)?;
    #[cfg(windows)]
    {
        let _ = std::fs::remove_file(auth_file);
    }
    std::fs::rename(&tmp, auth_file)?;
    // Re-assert on the final path (covers rename edge cases / FS quirks).
    // Best-effort: rename already published the new tokens.
    if let Err(e) = crate::util::secure_file::ensure_owner_only_permissions(auth_file) {
        tracing::warn!(
            error = %e,
            path = %auth_file.display(),
            "auth: failed to ensure owner-only permissions after rename"
        );
    }
    Ok(())
}

/// Non-atomic fallback: truncate and rewrite `auth.json` in place.
///
/// Used only when [`write_auth_json_atomic`] fails with `StorageFull`.
/// Opening with truncation first frees the old content's blocks before the
/// new bytes are written, so this needs only the file size in free space
/// rather than the temp-copy approach's file-size-of-headroom.
///
/// Truncation is destructive, so the prior bytes are snapshotted first and
/// restored best-effort if the rewrite fails partway — a failed fallback
/// must not leave an empty/torn file where a parseable (if stale) credential
/// used to be. A partial file that survives (because even the restore failed)
/// is healed on the next read via [`read_auth_json_or_empty_recovering_corrupt`].
fn write_auth_json_in_place(auth_file: &Path, auth_store: &AuthStore) -> std::io::Result<()> {
    write_auth_json_in_place_with(auth_file, auth_store, write_store_to)
}

/// Inner of [`write_auth_json_in_place`] with `write` injectable so the
/// rollback-on-failure path is unit-testable without an actually-full disk.
fn write_auth_json_in_place_with(
    auth_file: &Path,
    auth_store: &AuthStore,
    write: fn(&Path, &AuthStore) -> std::io::Result<()>,
) -> std::io::Result<()> {
    // Snapshot the prior bytes so a torn/empty write can be rolled back to
    // the previous on-disk credential. `None` when the file is absent.
    let prior = std::fs::read(auth_file).ok();
    match write(auth_file, auth_store) {
        Ok(()) => Ok(()),
        Err(e) => {
            if let Some(prior) = prior
                && let Err(restore_err) = restore_prior_bytes(auth_file, &prior)
            {
                tracing::warn!(
                    error = %restore_err,
                    "auth: failed to restore prior auth.json after in-place write failure"
                );
            }
            Err(e)
        }
    }
}

/// Best-effort rollback: rewrite `bytes` (owner-only, `fsync`'d) after a
/// failed in-place write so a torn/empty file does not replace the prior
/// credential.
fn restore_prior_bytes(auth_file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use crate::util::secure_file::open_secure_file;

    let mut file = open_secure_file(auth_file)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    crate::util::secure_file::ensure_owner_only_permissions(auth_file)?;
    Ok(())
}

/// Read a single auth token from `auth.json` by scope key.
/// Falls back to the legacy `https://accounts.x.ai/sign-in` scope key
/// when the requested scope is not found (devbox auth.json migration).
pub fn read_token_by_scope(grok_home: &Path, scope: &str) -> anyhow::Result<String> {
    let path = resolve_auth_json_path(grok_home);
    let store =
        read_auth_json(&path).map_err(|_| anyhow::anyhow!("Not logged in. Run `grok login`."))?;
    lookup_auth(&store, scope).map(|a| a.key).ok_or_else(|| {
        anyhow::anyhow!("Your auth token is invalid. Run `grok login` to re-authenticate.")
    })
}

/// Read the API key from the `xai::api_key` scope in auth.json.
pub fn read_api_key(grok_home: &Path) -> Option<String> {
    let path = resolve_auth_json_path(grok_home);
    let map = read_auth_json(&path).ok()?;
    map.get(API_KEY_SCOPE).map(|a| a.key.clone())
}

/// Store a plain API key in auth.json under the `xai::api_key` scope.
///
/// Uses the corrupt-recovery reader so a malformed auth.json (e.g. from a
/// previous crash) can be healed when the user sets an API key.
///
/// Serializes through `auth.json.lock` so a concurrent OAuth/platform writer
/// cannot be clobbered by a stale whole-map RMW.
pub fn store_api_key(grok_home: &Path, api_key: &str) -> std::io::Result<()> {
    let path = resolve_auth_json_path(grok_home);
    with_auth_json_scope_lock(&path, || {
        let mut map = read_auth_json_or_empty_recovering_corrupt(&path)?;
        map.insert(
            API_KEY_SCOPE.to_owned(),
            GrokAuth {
                key: api_key.to_owned(),
                auth_mode: AuthMode::ApiKey,
                ..Default::default()
            },
        );
        write_auth_json(&path, &map)
    })
}

/// Remove the `xai::api_key` scope from auth.json.
pub fn clear_api_key(grok_home: &Path) -> std::io::Result<()> {
    let path = resolve_auth_json_path(grok_home);
    with_auth_json_scope_lock(&path, || {
        if let Ok(mut map) = read_auth_json(&path) {
            map.remove(API_KEY_SCOPE);
            if map.is_empty() {
                let _ = std::fs::remove_file(&path);
            } else {
                write_auth_json(&path, &map)?;
            }
        }
        Ok(())
    })
}

/// Read the Kimi Code OAuth credential from `auth.json` (scope
/// [`KIMI_CODE_OAUTH_SCOPE`]).
pub fn read_kimi_code_auth(grok_home: &Path) -> Option<GrokAuth> {
    let path = resolve_auth_json_path(grok_home);
    let map = read_auth_json(&path).ok()?;
    let auth = map.get(KIMI_CODE_OAUTH_SCOPE)?.clone();
    (auth.auth_mode == AuthMode::KimiCode).then_some(auth)
}

/// Persist a Kimi Code OAuth credential under [`KIMI_CODE_OAUTH_SCOPE`].
/// Merges with existing scopes so xAI / OpenAI Codex login is preserved.
///
/// Serializes through `auth.json.lock` (same as Codex) so a concurrent
/// whole-map RMW cannot drop sibling scopes.
pub fn store_kimi_code_auth(grok_home: &Path, auth: &GrokAuth) -> std::io::Result<()> {
    let path = resolve_auth_json_path(grok_home);
    with_auth_json_scope_lock(&path, || {
        let mut map = read_auth_json_or_empty_recovering_corrupt(&path)?;
        let mut stored = auth.clone();
        stored.auth_mode = AuthMode::KimiCode;
        map.insert(KIMI_CODE_OAUTH_SCOPE.to_owned(), stored);
        write_auth_json(&path, &map)
    })
}

/// Remove the Kimi Code OAuth scope from auth.json.
pub fn clear_kimi_code_auth(grok_home: &Path) -> std::io::Result<()> {
    let path = resolve_auth_json_path(grok_home);
    with_auth_json_scope_lock(&path, || {
        if let Ok(mut map) = read_auth_json(&path) {
            map.remove(KIMI_CODE_OAUTH_SCOPE);
            if map.is_empty() {
                let _ = std::fs::remove_file(&path);
            } else {
                write_auth_json(&path, &map)?;
            }
        }
        Ok(())
    })
}

/// Read the OpenAI Codex (ChatGPT) OAuth credential, if present and correctly scoped.
pub fn read_openai_codex_auth(grok_home: &Path) -> Option<GrokAuth> {
    let path = resolve_auth_json_path(grok_home);
    let map = read_auth_json(&path).ok()?;
    let auth = map.get(OPENAI_CODEX_OAUTH_SCOPE)?.clone();
    (auth.auth_mode == AuthMode::OpenAiCodex).then_some(auth)
}

/// Persist an OpenAI Codex OAuth credential under [`OPENAI_CODEX_OAUTH_SCOPE`].
/// Merges with existing scopes so xAI login is preserved.
///
/// Serializes through `auth.json.lock` and re-reads under the lock so a
/// concurrent xAI/Kimi writer cannot be clobbered by a stale whole-map RMW.
pub fn store_openai_codex_auth(grok_home: &Path, auth: &GrokAuth) -> std::io::Result<()> {
    let path = resolve_auth_json_path(grok_home);
    with_auth_json_scope_lock(&path, || {
        let mut map = read_auth_json_or_empty_recovering_corrupt(&path)?;
        let mut stored = auth.clone();
        stored.auth_mode = AuthMode::OpenAiCodex;
        map.insert(OPENAI_CODEX_OAUTH_SCOPE.to_owned(), stored);
        write_auth_json(&path, &map)
    })
}

/// Like [`store_openai_codex_auth`], but if a sibling already rotated past
/// `spent_refresh` (their RT no longer matches the one we just spent), adopt
/// their on-disk entry instead of overwriting. Returns the credential that is
/// on disk after the call.
///
/// Important: a still-valid access token that still uses `spent_refresh` must
/// **not** block us — force-refresh after a 401 mints a new access token under
/// the same RT, and preferring the old access would re-send the rejected
/// bearer.
pub(crate) fn store_openai_codex_auth_after_refresh(
    grok_home: &Path,
    candidate: &GrokAuth,
    spent_refresh: &str,
) -> std::io::Result<GrokAuth> {
    let path = resolve_auth_json_path(grok_home);
    with_auth_json_scope_lock(&path, || {
        let mut map = read_auth_json_or_empty_recovering_corrupt(&path)?;
        if let Some(existing) = map.get(OPENAI_CODEX_OAUTH_SCOPE).cloned()
            && existing.auth_mode == AuthMode::OpenAiCodex
            && !super::model::is_expired(&existing)
        {
            let existing_rt = existing.refresh_token.as_deref().unwrap_or("");
            // Sibling already rotated past the RT we spent — prefer their write
            // so we do not clobber a newer family or double-spend.
            if existing_rt != spent_refresh {
                return Ok(existing);
            }
            // Same RT, same access: idempotent no-op.
            if existing.key == candidate.key {
                return Ok(existing);
            }
            // Same RT, different access: we just minted a replacement (e.g.
            // force-refresh after 401) — fall through and persist candidate.
        }
        let mut stored = candidate.clone();
        stored.auth_mode = AuthMode::OpenAiCodex;
        map.insert(OPENAI_CODEX_OAUTH_SCOPE.to_owned(), stored.clone());
        write_auth_json(&path, &map)?;
        Ok(stored)
    })
}

/// Remove the OpenAI Codex OAuth scope from auth.json.
pub fn clear_openai_codex_auth(grok_home: &Path) -> std::io::Result<()> {
    let path = resolve_auth_json_path(grok_home);
    with_auth_json_scope_lock(&path, || {
        if let Ok(mut map) = read_auth_json(&path) {
            map.remove(OPENAI_CODEX_OAUTH_SCOPE);
            if map.is_empty() {
                let _ = std::fs::remove_file(&path);
            } else {
                write_auth_json(&path, &map)?;
            }
        }
        Ok(())
    })
}

/// Hold an exclusive flock on `auth.json.lock` for a short disk RMW.
///
/// Blocking is intentional: these writers only touch disk (no network under
/// the lock). Writes `PID:TS` holder info so the AuthManager stale-lock path
/// can still identify the holder.
fn with_auth_json_scope_lock<R>(auth_json_path: &Path, f: impl FnOnce() -> R) -> R {
    let lock_path = auth_json_path.with_file_name("auth.json.lock");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .ok();
    if let Some(ref mut file) = file {
        if let Err(e) = file.lock_exclusive() {
            tracing::warn!(
                error = %e,
                path = %lock_path.display(),
                "auth: could not flock auth.json.lock for scope write; proceeding unlocked"
            );
        } else if let Err(e) = write_scope_lock_holder_info(file) {
            tracing::warn!(
                error = %e,
                "auth: failed to write auth.json.lock holder info"
            );
        }
    } else {
        tracing::warn!(
            path = %lock_path.display(),
            "auth: could not open auth.json.lock for scope write; proceeding unlocked"
        );
    }
    let out = f();
    drop(file); // unlock
    out
}

fn write_scope_lock_holder_info(file: &mut File) -> std::io::Result<()> {
    let pid = std::process::id();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    file.set_len(0)?;
    file.seek(std::io::SeekFrom::Start(0))?;
    write!(file, "{pid}:{ts}")?;
    file.sync_all()?;
    Ok(())
}

/// Read a third-party platform API key from `auth.json` (`platform/<id>`).
///
/// Set by the TUI `/providers` flow. Never log the returned value.
pub fn read_platform_api_key(grok_home: &Path, platform: &str) -> Option<String> {
    let path = resolve_auth_json_path(grok_home);
    let map = read_auth_json(&path).ok()?;
    let auth = map.get(&platform_api_key_scope(platform))?;
    let key = auth.key.trim();
    if key.is_empty() {
        None
    } else {
        Some(key.to_owned())
    }
}

/// Persist a third-party platform API key under `platform/<id>`.
///
/// Empty/`clear` removes the scope. Merges with existing scopes so xAI / Kimi
/// OAuth sessions are preserved.
///
/// Serializes through `auth.json.lock` (same as Kimi/Codex) so a concurrent
/// whole-map RMW cannot drop sibling scopes.
pub fn store_platform_api_key(
    grok_home: &Path,
    platform: &str,
    api_key: &str,
) -> std::io::Result<()> {
    let trimmed = api_key.trim();
    if trimmed.is_empty() {
        return clear_platform_api_key(grok_home, platform);
    }
    let path = resolve_auth_json_path(grok_home);
    with_auth_json_scope_lock(&path, || {
        let mut map = read_auth_json_or_empty_recovering_corrupt(&path)?;
        map.insert(
            platform_api_key_scope(platform),
            GrokAuth {
                key: trimmed.to_owned(),
                auth_mode: AuthMode::ApiKey,
                ..Default::default()
            },
        );
        write_auth_json(&path, &map)
    })
}

/// Remove a platform API key scope from auth.json.
pub fn clear_platform_api_key(grok_home: &Path, platform: &str) -> std::io::Result<()> {
    let path = resolve_auth_json_path(grok_home);
    with_auth_json_scope_lock(&path, || {
        if let Ok(mut map) = read_auth_json(&path) {
            map.remove(&platform_api_key_scope(platform));
            if map.is_empty() {
                let _ = std::fs::remove_file(&path);
            } else {
                write_auth_json(&path, &map)?;
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod platform_api_key_tests {
    use super::*;

    #[test]
    fn platform_api_key_roundtrips_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        assert!(read_platform_api_key(home, "zai").is_none());
        store_platform_api_key(home, "zai", " sk-zai-test ").unwrap();
        assert_eq!(
            read_platform_api_key(home, "zai").as_deref(),
            Some("sk-zai-test")
        );
        // Sibling scopes preserved.
        store_api_key(home, "xai-key").unwrap();
        assert_eq!(read_api_key(home).as_deref(), Some("xai-key"));
        assert_eq!(
            read_platform_api_key(home, "zai").as_deref(),
            Some("sk-zai-test")
        );
        store_platform_api_key(home, "zai", "").unwrap();
        assert!(read_platform_api_key(home, "zai").is_none());
        assert_eq!(read_api_key(home).as_deref(), Some("xai-key"));
    }
}

#[cfg(test)]
mod grok_auth_path_tests {
    use super::*;
    use crate::auth::model::{AuthMode, GrokAuth};
    use serial_test::serial;
    use xai_grok_test_support::EnvGuard;

    /// Login/store helpers must honor `GROK_AUTH_PATH` pointing at a non-default
    /// basename (e.g. `scratch.json`), matching the refresh path that already
    /// calls [`auth_json_path`].
    #[test]
    #[serial]
    fn openai_codex_store_roundtrips_through_grok_auth_path() {
        let dir = tempfile::tempdir().unwrap();
        let auth_file = dir.path().join("scratch.json");
        let _guard = EnvGuard::set("GROK_AUTH_PATH", auth_file.to_str().unwrap());

        let path = auth_json_path();
        assert_eq!(path, auth_file);
        let home = path.parent().unwrap();

        let auth = GrokAuth {
            key: "codex-access-token".to_owned(),
            refresh_token: Some("codex-refresh".to_owned()),
            auth_mode: AuthMode::OpenAiCodex,
            email: Some("user@example.com".to_owned()),
            ..Default::default()
        };
        store_openai_codex_auth(home, &auth).unwrap();

        let loaded = read_openai_codex_auth(home).expect("token should be on GROK_AUTH_PATH");
        assert_eq!(loaded.key, "codex-access-token");
        assert_eq!(loaded.refresh_token.as_deref(), Some("codex-refresh"));
        assert!(auth_file.is_file(), "credential written to GROK_AUTH_PATH");
        // Default ~/.grok/auth.json must not have been touched by this write path.
        // (We cannot assert absence of the real home file; only that our scratch exists.)
    }

    #[test]
    #[serial]
    fn kimi_store_roundtrips_through_grok_auth_path() {
        let dir = tempfile::tempdir().unwrap();
        let auth_file = dir.path().join("isolated-auth.json");
        let _guard = EnvGuard::set("GROK_AUTH_PATH", auth_file.to_str().unwrap());

        let path = auth_json_path();
        let home = path.parent().unwrap();

        let auth = GrokAuth {
            key: "kimi-access".to_owned(),
            refresh_token: Some("kimi-refresh".to_owned()),
            auth_mode: AuthMode::KimiCode,
            ..Default::default()
        };
        store_kimi_code_auth(home, &auth).unwrap();
        let loaded = read_kimi_code_auth(home).expect("kimi token on GROK_AUTH_PATH");
        assert_eq!(loaded.key, "kimi-access");
        clear_kimi_code_auth(home).unwrap();
        assert!(read_kimi_code_auth(home).is_none());
    }
}

#[cfg(test)]
mod write_fallback_tests {
    use super::*;

    fn sample_store() -> AuthStore {
        let mut map = AuthStore::new();
        map.insert(
            API_KEY_SCOPE.to_owned(),
            GrokAuth {
                key: "secret-key".to_owned(),
                auth_mode: AuthMode::ApiKey,
                ..Default::default()
            },
        );
        map
    }

    fn read_key(path: &Path) -> Option<String> {
        read_auth_json(path)
            .ok()
            .and_then(|m| m.get(API_KEY_SCOPE).map(|a| a.key.clone()))
    }

    fn fake_storage_full(_: &Path, _: &AuthStore) -> std::io::Result<()> {
        Err(std::io::Error::from(std::io::ErrorKind::StorageFull))
    }

    fn fake_permission_denied(_: &Path, _: &AuthStore) -> std::io::Result<()> {
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    }

    /// Simulates an in-place write that truncates the file (destroying the
    /// old content, as `open_secure_file` does) and then fails partway — the
    /// torn-write case the rollback must recover from.
    fn fake_truncate_then_fail(path: &Path, _: &AuthStore) -> std::io::Result<()> {
        crate::util::secure_file::open_secure_file(path)?; // truncates to 0 bytes
        Err(std::io::Error::from(std::io::ErrorKind::StorageFull))
    }

    #[test]
    fn in_place_write_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        write_auth_json_in_place(&path, &sample_store()).unwrap();
        assert_eq!(read_key(&path).as_deref(), Some("secret-key"));
    }

    #[cfg(unix)]
    #[test]
    fn in_place_write_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        write_auth_json_in_place(&path, &sample_store()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "in-place write must stay 0o600");
    }

    #[cfg(unix)]
    #[test]
    fn write_tightens_preexisting_world_readable_auth_json() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(&path, b"{}").unwrap();
        let mut loose = std::fs::metadata(&path).unwrap().permissions();
        loose.set_mode(0o644);
        std::fs::set_permissions(&path, loose).unwrap();

        write_auth_json(&path, &sample_store()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "rewrite must tighten preexisting open perms"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_tightens_world_readable_auth_json() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        write_auth_json(&path, &sample_store()).unwrap();
        let mut loose = std::fs::metadata(&path).unwrap().permissions();
        loose.set_mode(0o644);
        std::fs::set_permissions(&path, loose).unwrap();

        let _ = read_auth_json(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "load must tighten open auth.json perms"
        );
    }

    /// A `StorageFull` (ENOSPC) failure on the atomic path must fall back to
    /// the in-place write so the credential still lands on disk.
    #[test]
    fn falls_back_to_in_place_on_storage_full() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        write_auth_json_with(&path, &sample_store(), fake_storage_full).unwrap();
        assert_eq!(
            read_key(&path).as_deref(),
            Some("secret-key"),
            "disk-full atomic write must fall back to a successful in-place write"
        );
    }

    /// Non-ENOSPC errors must propagate unchanged and must NOT trigger the
    /// in-place fallback (e.g. a permission error should not write the file).
    #[test]
    fn propagates_non_storage_full_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let err = write_auth_json_with(&path, &sample_store(), fake_permission_denied).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(!path.exists(), "non-ENOSPC failure must not write the file");
    }

    /// The normal (real atomic) path still works end to end.
    #[test]
    fn atomic_write_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        write_auth_json(&path, &sample_store()).unwrap();
        assert_eq!(read_key(&path).as_deref(), Some("secret-key"));
    }

    /// A fallback write that truncates then fails must roll back to the prior
    /// bytes instead of leaving an empty/torn file — otherwise a second
    /// disk-full failure would destroy a previously-valid credential.
    #[test]
    fn in_place_restores_prior_bytes_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        // Seed a valid prior credential.
        write_auth_json_in_place(&path, &sample_store()).unwrap();
        assert_eq!(read_key(&path).as_deref(), Some("secret-key"));

        let mut replacement = AuthStore::new();
        replacement.insert(
            API_KEY_SCOPE.to_owned(),
            GrokAuth {
                key: "replacement-key".to_owned(),
                auth_mode: AuthMode::ApiKey,
                ..Default::default()
            },
        );
        let err = write_auth_json_in_place_with(&path, &replacement, fake_truncate_then_fail)
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::StorageFull);
        assert_eq!(
            read_key(&path).as_deref(),
            Some("secret-key"),
            "a failed in-place write must restore the prior credential, not leave an empty file"
        );
    }

    /// Rollback after a failed write must keep the file owner-only (0o600).
    #[cfg(unix)]
    #[test]
    fn in_place_restore_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        write_auth_json_in_place(&path, &sample_store()).unwrap();
        let _ = write_auth_json_in_place_with(&path, &sample_store(), fake_truncate_then_fail);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "restored file must stay 0o600");
    }
}
