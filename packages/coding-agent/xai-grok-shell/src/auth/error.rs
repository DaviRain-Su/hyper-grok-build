use std::borrow::Cow;

use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AuthError {
    #[error("Not logged in. Run `grok login`.")]
    NotLoggedIn,

    /// Token expired and no refresh authority available.
    #[error("Token expired. Run `grok login` to re-authenticate.")]
    TokenExpiredNoRefresh,

    /// Server rejected the token (401) with no recovery path.
    #[error("Authentication rejected by server. Run `grok login` to re-authenticate.")]
    ServerRejectedNoRecovery,

    /// All recovery strategies exhausted.
    #[error("Auth recovery exhausted; re-authentication required.")]
    RecoveryExhausted,

    /// A session's team principal violates the `force_login_team_uuid` pin.
    /// `message` states which team is required vs. returned.
    #[error("{message} Run `grok login` to sign in with the required team.")]
    PinnedTeamMismatch { message: String },

    /// Cached API-key session rejected because API-key auth is disabled.
    #[error("API-key auth is disabled by your administrator. Run `grok login` to authenticate.")]
    ApiKeyAuthDisabled,

    /// Outcome of a refresh-authority attempt. Recoverability (and, for
    /// permanent failures, the reason) lives in [`RefreshTokenError`].
    #[error(transparent)]
    Refresh(#[from] RefreshTokenError),
}

/// Recoverability axis of a token-refresh attempt. Deliberately total (no
/// `#[non_exhaustive]`): "permanent vs transient" is a closed decision every
/// caller must make, so a future third state should break consumers loudly.
#[derive(Debug, Error)]
pub enum RefreshTokenError {
    /// The credential is dead; the user must re-authenticate.
    #[error(transparent)]
    Permanent(#[from] RefreshTokenFailedError),
    /// Network / 5xx / unknown blip; safe to retry later. Carries the cause.
    #[error(transparent)]
    Transient(RefreshTransientError),
}

/// A retryable refresh failure, wrapping its cause. No public `From`:
/// construct only via [`AuthError::transient`] /
/// [`AuthError::transient_source`], so a stray `?` on some error can't silently
/// classify a permanent failure as retryable (mirrors the dedicated
/// [`RefreshTokenFailedError`] on the permanent arm). Display frames the cause
/// as an auth-refresh failure so internal messages (lock timeout, sleep defer)
/// don't surface bare; the permanent arm derives its copy from
/// [`RefreshTokenFailedReason::user_message`] and is not prefixed.
#[derive(Debug, Error)]
#[error("auth refresh failed: {source}")]
pub struct RefreshTransientError {
    #[source]
    source: Box<dyn std::error::Error + Send + Sync>,
    reason: TransientReason,
}

/// Why the refresh path returned a transient error — machine-readable so
/// callers can branch and telemetry can count the deferral classes apart
/// (they all share one `Transient` message otherwise). `Other` covers
/// causes carried in the source error (network, 5xx, persist failures).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransientReason {
    /// Deferred: system sleep imminent (sleep gate raised).
    SleepGate,
    /// Deferred: dark wake (background consumer, or user-facing within the
    /// deferral budget).
    DarkWakeDeferred,
    /// Deferred: consumed-RT sentinel present; retrying only at full wake.
    SentinelAwaitingWake,
    /// This failure straddled a suspend and recorded the suspect-RT
    /// sentinel; the retry goes through the gate's election.
    StraddleSuspectRecorded,
    /// Aborted: the `auth.json.lock` died between the sentinel election and
    /// the retry stamp; retrying re-runs the election under a fresh lock.
    SentinelLockLost,
    /// Aborted: the sentinel retry stamp could not be written; a process
    /// that cannot write the stamp must not present the suspect RT.
    SentinelStampFailed,
    /// Deferred: another process holds the sentinel retry election.
    SentinelCooldown,
    /// No refresher configured.
    NoRefresher,
    /// `auth.json.lock` could not be acquired (or re-acquired) in time.
    LockTimeout,
    /// A sibling rotated the credential mid-flight; adopt on retry.
    SiblingRotation,
    /// Cause lives in the error chain (network, 5xx, persist failure, …).
    Other,
}

impl TransientReason {
    /// Stable label for telemetry fields.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::SleepGate => "sleep_gate",
            Self::DarkWakeDeferred => "dark_wake_deferred",
            Self::SentinelAwaitingWake => "sentinel_awaiting_wake",
            Self::StraddleSuspectRecorded => "straddle_suspect_recorded",
            Self::SentinelLockLost => "sentinel_lock_lost",
            Self::SentinelStampFailed => "sentinel_stamp_failed",
            Self::SentinelCooldown => "sentinel_cooldown",
            Self::NoRefresher => "no_refresher",
            Self::LockTimeout => "lock_timeout",
            Self::SiblingRotation => "sibling_rotation",
            Self::Other => "other",
        }
    }
}

/// A terminal refresh failure. `reason` is machine-readable; the user-facing
/// copy is derived from it via [`RefreshTokenFailedReason::user_message`], so
/// the two can never drift.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{}", .reason.user_message())]
#[non_exhaustive]
pub struct RefreshTokenFailedError {
    pub reason: RefreshTokenFailedReason,
}

impl From<RefreshTokenFailedReason> for RefreshTokenFailedError {
    fn from(reason: RefreshTokenFailedReason) -> Self {
        Self { reason }
    }
}

/// Why a token refresh terminally failed, grounded in the OAuth2 error codes
/// our IdP actually emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefreshTokenFailedReason {
    /// `invalid_grant` — the refresh token is no longer valid (expired, reused,
    /// or revoked; the IdP does not distinguish these).
    RefreshTokenRejected,
    /// `invalid_client` — the client/app credential was rejected.
    ClientRejected,
    /// The operator's `auth_provider_command` could not mint a credential in a
    /// headless run (`GROK_AUTH_EXPIRED=1`).
    ProviderInteractiveRequired,
    /// Escalation from repeated transient failures (OIDC). Never a raw IdP
    /// code: an unrecognized terminal code is classified transient, not
    /// `Other` (see `classify_terminal`).
    Other,
}

impl RefreshTokenFailedReason {
    /// Sticky until the credential changes (never ages out): a revoked refresh
    /// token never self-heals, whereas client rotation / transient escalation
    /// recover, so those age out past the TTL.
    pub(crate) fn is_sticky(self) -> bool {
        match self {
            Self::RefreshTokenRejected => true,
            Self::ClientRejected | Self::ProviderInteractiveRequired | Self::Other => false,
        }
    }

    /// Whether the verdict rules out an unattended retry for as long as it
    /// stands. Orthogonal to [`Self::is_sticky`], which is about whether the
    /// verdict ever ages out.
    pub(crate) fn blocks_unattended_retry(self) -> bool {
        match self {
            Self::RefreshTokenRejected | Self::ProviderInteractiveRequired => true,
            Self::ClientRejected | Self::Other => false,
        }
    }

    /// User-facing copy for a terminal refresh failure; the raw IdP code stays
    /// in logs.
    pub(crate) fn user_message(self) -> Cow<'static, str> {
        match self {
            Self::RefreshTokenRejected => {
                "Your session has expired. Run `grok login` to sign in again.".into()
            }
            Self::ClientRejected => {
                "Authentication is temporarily unavailable. Run `grok login` if this persists."
                    .into()
            }
            Self::ProviderInteractiveRequired => provider_login_message(None),
            Self::Other => {
                "Authentication could not be refreshed. Run `grok login` to sign in again.".into()
            }
        }
    }
}

/// `label` is the operator's `auth_provider_label`, where the surface has one.
pub(crate) fn provider_login_message(label: Option<&str>) -> Cow<'static, str> {
    match label {
        Some(label) => format!(
            "Your session expired and {label} could not renew it in the background. \
             Run /login to sign in again."
        )
        .into(),
        None => "Your session expired and your sign-in helper could not renew it in the \
                 background. Run /login to sign in again."
            .into(),
    }
}

impl AuthError {
    /// A retryable refresh failure with a message-only cause and
    /// [`TransientReason::Other`]; prefer [`Self::transient_reason`] at the
    /// refresh path's deferral/lock sites so telemetry can count them apart.
    pub(crate) fn transient(message: impl Into<String>) -> Self {
        Self::transient_source(message.into())
    }

    /// [`Self::transient`] with an explicit machine-readable reason.
    pub(crate) fn transient_reason(reason: TransientReason, message: impl Into<String>) -> Self {
        AuthError::Refresh(RefreshTokenError::Transient(RefreshTransientError {
            source: message.into().into(),
            reason,
        }))
    }

    /// A retryable refresh failure that preserves `source` in the error chain
    /// (`Transient` carries the cause), so callers with a real error don't
    /// flatten it to a string.
    pub(crate) fn transient_source(
        source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        AuthError::Refresh(RefreshTokenError::Transient(RefreshTransientError {
            source: source.into(),
            reason: TransientReason::Other,
        }))
    }

    /// The transient reason, `None` for non-transient errors.
    pub(crate) fn transient_reason_kind(&self) -> Option<TransientReason> {
        match self {
            AuthError::Refresh(RefreshTokenError::Transient(t)) => Some(t.reason),
            _ => None,
        }
    }

    /// A terminal refresh failure for an already-classified `reason`.
    pub(crate) fn permanent(reason: RefreshTokenFailedReason) -> Self {
        AuthError::Refresh(RefreshTokenError::Permanent(reason.into()))
    }

    /// Retryable refresh failure (network, 5xx, sleep/dark-wake defer, etc.).
    /// Permanent failures, NotLoggedIn, and policy rejects are not transient.
    pub(crate) fn is_transient(&self) -> bool {
        matches!(self, AuthError::Refresh(RefreshTokenError::Transient(_)))
    }
}
