//! [`RetryPolicy`] — maps a non-2xx HTTP status code to a [`Disposition`],
//! consolidating the scattered "what should I do with this response" logic.
//!
//! Two named presets:
//! - [`RetryPolicy::server`] — server-side preset: retry on 429 or any 5xx;
//!   all other non-2xx are terminal.
//! - [`RetryPolicy::client_storage`] — client upload/storage preset:
//!   400/403/404 terminal-drop, 401 auth-refresh-once, everything else retried.

/// What a caller should do with a non-2xx HTTP response, by status code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// transient: retry with backoff (5xx, 429, etc.)
    Retryable,
    /// refresh credentials once, then give up (e.g. 401)
    AuthRefresh,
    /// permanent: drop immediately, never retry (e.g. 400/403/404)
    Terminal,
}

/// Maps an HTTP status code to a [`Disposition`].
pub struct RetryPolicy {
    retryable: &'static [u16],
    auth_refresh: &'static [u16],
    terminal: &'static [u16],
    default: Disposition,
}

impl RetryPolicy {
    /// Classify `status`. Returns `None` for 2xx (success, not an error).
    pub fn classify(&self, status: u16) -> Option<Disposition> {
        if (200..300).contains(&status) {
            return None;
        }
        if self.auth_refresh.contains(&status) {
            return Some(Disposition::AuthRefresh);
        }
        if self.terminal.contains(&status) {
            return Some(Disposition::Terminal);
        }
        if self.retryable.contains(&status) || (500..600).contains(&status) {
            return Some(Disposition::Retryable);
        }
        Some(self.default)
    }

    /// `true` iff `status` classifies as `Retryable`. This is what an HTTP
    /// server emits in an `x-should-retry` header.
    pub fn should_retry(&self, status: u16) -> bool {
        matches!(self.classify(status), Some(Disposition::Retryable))
    }

    /// Server preset: 429 and any 5xx are retryable, everything else is
    /// terminal.
    pub const fn server() -> Self {
        Self {
            retryable: &[429],
            auth_refresh: &[],
            terminal: &[],
            default: Disposition::Terminal,
        }
    }

    /// Client storage/upload preset: 400/403/404 terminal-drop, 401
    /// auth-refresh-once, everything else (429, 5xx, unlisted 4xx) retried.
    pub const fn client_storage() -> Self {
        Self {
            retryable: &[],
            auth_refresh: &[401],
            terminal: &[400, 403, 404],
            default: Disposition::Retryable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_should_retry() {
        let policy = RetryPolicy::server();
        for code in [429, 500, 502, 503, 504, 501, 520] {
            assert!(policy.should_retry(code), "expected {code} to retry");
        }
        for code in [400, 401, 403, 404, 200] {
            assert!(!policy.should_retry(code), "expected {code} to NOT retry");
        }
        assert_eq!(policy.classify(200), None);
    }

    #[test]
    fn client_storage_classify() {
        let policy = RetryPolicy::client_storage();
        for code in [400, 403, 404] {
            assert_eq!(policy.classify(code), Some(Disposition::Terminal));
        }
        assert_eq!(policy.classify(401), Some(Disposition::AuthRefresh));
        for code in [429, 500, 503, 409, 422] {
            assert_eq!(policy.classify(code), Some(Disposition::Retryable));
        }
        assert_eq!(policy.classify(200), None);
    }
}
