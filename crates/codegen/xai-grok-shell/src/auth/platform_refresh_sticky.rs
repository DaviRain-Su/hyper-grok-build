//! Process-local sticky permanent-failure cache for third-party OAuth refresh.
//!
//! xAI [`super::manager::AuthManager`] already records sticky `invalid_grant`
//! verdicts so a dead refresh token does not re-hit the IdP every turn.
//! Kimi Code and OpenAI Codex use independent refresh paths; without an
//! equivalent cache, a revoked RT produces a force-refresh storm on every
//! sampler 401.
//!
//! Verdicts are keyed by **refresh-token identity** (not access token) so a
//! successful re-login that mints a new RT is not blocked by a prior failure.
//! Transient network / 5xx failures are never recorded.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// How long a permanent refresh failure stays sticky before another attempt.
const STICKY_TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PlatformRefreshFamily {
    KimiCode,
    OpenAiCodex,
    AnthropicClaude,
}

#[derive(Debug, Clone)]
struct StickyVerdict {
    recorded_at: Instant,
    reason: String,
}

static STICKY: LazyLock<Mutex<HashMap<(PlatformRefreshFamily, String), StickyVerdict>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn fingerprint_refresh_token(refresh_token: &str) -> String {
    // Full RT must never be stored; a short prefix is enough to scope the
    // verdict and for log correlation (mirrors sampler 401 attribution).
    // Slice by *chars*, not bytes — a multi-byte UTF-8 boundary at index 16
    // would panic on `str` indexing.
    refresh_token.trim().chars().take(16).collect()
}

/// Return the cached permanent-failure reason for this RT, if still within TTL.
pub(crate) fn sticky_permanent_failure(
    family: PlatformRefreshFamily,
    refresh_token: &str,
) -> Option<String> {
    let key = (family, fingerprint_refresh_token(refresh_token));
    let mut map = STICKY.lock().unwrap_or_else(|e| e.into_inner());
    let Some(verdict) = map.get(&key) else {
        return None;
    };
    if verdict.recorded_at.elapsed() >= STICKY_TTL {
        map.remove(&key);
        return None;
    }
    Some(verdict.reason.clone())
}

/// Record a permanent (non-retryable) refresh failure for this RT.
pub(crate) fn record_sticky_permanent_failure(
    family: PlatformRefreshFamily,
    refresh_token: &str,
    reason: impl Into<String>,
) {
    let key = (family, fingerprint_refresh_token(refresh_token));
    let reason = reason.into();
    let mut map = STICKY.lock().unwrap_or_else(|e| e.into_inner());
    map.insert(
        key,
        StickyVerdict {
            recorded_at: Instant::now(),
            reason,
        },
    );
}

/// Clear sticky state for one RT (e.g. after a successful refresh under a new family).
pub(crate) fn clear_sticky_for_refresh_token(family: PlatformRefreshFamily, refresh_token: &str) {
    let key = (family, fingerprint_refresh_token(refresh_token));
    let mut map = STICKY.lock().unwrap_or_else(|e| e.into_inner());
    map.remove(&key);
}

/// Clear all sticky verdicts for a platform family (logout / re-login).
pub(crate) fn clear_sticky_family(family: PlatformRefreshFamily) {
    let mut map = STICKY.lock().unwrap_or_else(|e| e.into_inner());
    map.retain(|(f, _), _| *f != family);
}

/// Whether a Kimi refresh error should stick (401/403 unauthorized / invalid grant).
pub(crate) fn kimi_refresh_error_is_permanent(err: &super::kimi::oauth::RefreshError) -> bool {
    use super::kimi::oauth::RefreshError;
    match err {
        RefreshError::Unauthorized { .. } => true,
        RefreshError::Fatal {
            status,
            description,
        } => {
            // Only 4xx invalid_grant-style fatals stick; 5xx/Exhausted do not.
            (*status == 400 || *status == 401 || *status == 403)
                && description.to_ascii_lowercase().contains("invalid_grant")
        }
        RefreshError::Exhausted(_) | RefreshError::Other(_) => false,
    }
}

/// Whether a Codex refresh `anyhow` error should stick.
///
/// Prefer structured status from the error message prefix
/// (`… failed (HTTP {status}): …`) so a 5xx body that happens to contain
/// "unauthorized" is never sticky. Timeouts remain transient.
pub(crate) fn codex_refresh_error_is_permanent(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}");
    let lower = msg.to_ascii_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        return false;
    }
    // Structured path: "OpenAI Codex token refresh failed (HTTP NNN): …"
    if let Some(status) = parse_http_status_from_codex_error(&msg) {
        if status == 401 || status == 403 {
            return true;
        }
        if status == 400 && lower.contains("invalid_grant") {
            return true;
        }
        // 5xx and other statuses: never sticky, even if the body mentions 401.
        return false;
    }
    // Fallback when status is missing from the error string.
    lower.contains("invalid_grant")
}

fn parse_http_status_from_codex_error(msg: &str) -> Option<u16> {
    // Match "(HTTP 401)" / "(HTTP 403)" style prefixes from read_token_response.
    let marker = "(HTTP ";
    let start = msg.find(marker)? + marker.len();
    let end = msg[start..].find(')')? + start;
    msg[start..end].trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sticky_scopes_to_refresh_token_fingerprint() {
        clear_sticky_family(PlatformRefreshFamily::KimiCode);
        record_sticky_permanent_failure(
            PlatformRefreshFamily::KimiCode,
            "rt-dead-token-aaaaaaaa",
            "invalid_grant",
        );
        assert!(
            sticky_permanent_failure(PlatformRefreshFamily::KimiCode, "rt-dead-token-aaaaaaaa")
                .is_some()
        );
        assert!(
            sticky_permanent_failure(PlatformRefreshFamily::KimiCode, "rt-other-token-bbbbbbbb")
                .is_none(),
            "different RT must not inherit the sticky verdict"
        );
        assert!(
            sticky_permanent_failure(PlatformRefreshFamily::OpenAiCodex, "rt-dead-token-aaaaaaaa")
                .is_none(),
            "different family must not share the verdict"
        );
        clear_sticky_family(PlatformRefreshFamily::KimiCode);
    }

    #[test]
    fn clear_family_removes_verdicts() {
        clear_sticky_family(PlatformRefreshFamily::OpenAiCodex);
        record_sticky_permanent_failure(
            PlatformRefreshFamily::OpenAiCodex,
            "rt-codex-dead-11111111",
            "401",
        );
        clear_sticky_family(PlatformRefreshFamily::OpenAiCodex);
        assert!(
            sticky_permanent_failure(PlatformRefreshFamily::OpenAiCodex, "rt-codex-dead-11111111")
                .is_none()
        );
    }

    #[test]
    fn kimi_permanent_classifier() {
        use super::super::kimi::oauth::RefreshError;
        assert!(kimi_refresh_error_is_permanent(
            &RefreshError::Unauthorized {
                status: 401,
                description: "bad".into(),
            }
        ));
        assert!(kimi_refresh_error_is_permanent(&RefreshError::Fatal {
            status: 400,
            description: "invalid_grant: expired".into(),
        }));
        assert!(!kimi_refresh_error_is_permanent(&RefreshError::Exhausted(
            "5xx".into()
        )));
        assert!(!kimi_refresh_error_is_permanent(&RefreshError::Fatal {
            status: 500,
            description: "server error".into(),
        }));
    }

    #[test]
    fn codex_permanent_classifier() {
        assert!(codex_refresh_error_is_permanent(&anyhow::anyhow!(
            "OpenAI Codex token refresh failed (HTTP 401): expired"
        )));
        assert!(codex_refresh_error_is_permanent(&anyhow::anyhow!(
            "OpenAI Codex token refresh failed (HTTP 400): invalid_grant"
        )));
        // 5xx body mentioning "unauthorized" must NOT stick.
        assert!(!codex_refresh_error_is_permanent(&anyhow::anyhow!(
            "OpenAI Codex token refresh failed (HTTP 500): unauthorized_internal"
        )));
        assert!(!codex_refresh_error_is_permanent(&anyhow::anyhow!(
            "token refresh timed out after 15s"
        )));
        assert!(!codex_refresh_error_is_permanent(&anyhow::anyhow!(
            "connection reset by peer"
        )));
        // Fallback when status is absent: invalid_grant alone still sticks.
        assert!(codex_refresh_error_is_permanent(&anyhow::anyhow!(
            "token refresh failed: invalid_grant"
        )));
    }

    #[test]
    fn fingerprint_handles_short_and_multibyte() {
        assert_eq!(fingerprint_refresh_token("short"), "short");
        assert_eq!(fingerprint_refresh_token(""), "");
        // Multi-byte chars must not panic on the 16-char cut.
        let wide = "αβγδεζηθικλμνξοπρστυ"; // >16 chars
        let fp = fingerprint_refresh_token(wide);
        assert_eq!(fp.chars().count(), 16);
    }
}
