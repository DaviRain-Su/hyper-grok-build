//! HTTP clients for the application.
//!
//! Building a `reqwest::Client` is expensive (~95ms) because it loads
//! TLS root certificates from the OS trust store. This module
//! provides four clients for non-sampling traffic (the first three
//! public and cached, the last crate-internal and built on demand):
//!
//! - `shared_client` -- a `OnceLock`-cached async client for general
//!   use (telemetry, feedback, settings, etc.).
//! - `shared_upload_client` -- a `OnceLock`-cached client for GCS
//!   uploads with aggressive connection pool eviction.
//! - `shared_blocking_client` -- a blocking client for the early
//!   model prefetch (runs before the async runtime is available).
//! - `fresh_http1_client` -- a crate-internal, on-demand, pool-less
//!   HTTP/1.1 client used by `send_with_retry_escaping_pool` for the
//!   final retry attempt to escape a poisoned pool within a tight budget.
//!
//! Sampling traffic uses process-wide shared clients owned by
//! `xai_grok_sampler::shared_http` (one HTTP/2 pooled client plus
//! a pool-less HTTP/1.1 fallback shared across every
//! `SamplingClient`). The sampler reads `GROK_POOL_*` /
//! `GROK_CONNECT_TIMEOUT_SECS` once, when its shared client is
//! first built, and `GROK_SAMPLER_SHARED_CLIENT=0` falls back to
//! a fresh client per `SamplingClient`.
//!
//! TLS root certificates are warmed at process start via
//! `warm_async_http_client()` (in `mvp_agent.rs`).

use std::sync::OnceLock;

use xai_grok_workspace::permission::ClientType;

/// Startup span timer, local to this crate.
///
/// Replaces `xai_grok_shell::instrumentation_timer!`, which cannot be referenced
/// here (it lives in the shell crate, which now depends on this one). This is a
/// behavior-preserving copy: it routes to the same
/// `xai_grok_telemetry::instrumentation` API and keeps the Chrome trace
/// span for these startup timings.
macro_rules! startup_timer {
    ($name:literal) => {{
        use xai_grok_telemetry::instrumentation::{
            InstrumentationMode, InstrumentationTimer, TARGET, current_mode,
        };
        let mode = current_mode();
        match mode {
            InstrumentationMode::Chrome => {
                let span = tracing::info_span!(target: TARGET, $name);
                InstrumentationTimer::new_with_span($name, mode, Some(span.entered()))
            }
            _ => InstrumentationTimer::new($name),
        }
    }};
}

static CLIENT_TYPE: OnceLock<ClientType> = OnceLock::new();

// `OriginClientInfo` is owned by `xai-grok-sampler` so `SamplerConfig` can use
// it without taking a circular dependency on `xai-grok-shell`. Re-exported
// under the same path (`crate::http::OriginClientInfo`) so existing call-sites
// compile unchanged. The telemetry engine in `xai-grok-telemetry` consumes
// the same type via `xai_grok_sampler::OriginClientInfo`. The shell-specific
// constructors that depended on `ClientType` (a shell-only type) are free
// functions below.
pub use xai_grok_sampler::OriginClientInfo;

/// Construct an [`OriginClientInfo`] from `GROK_CLIENT_NAME` /
/// `GROK_CLIENT_VERSION` env vars. Returns `None` when
/// `GROK_CLIENT_NAME` is unset.
pub fn origin_client_info_from_env() -> Option<OriginClientInfo> {
    std::env::var("GROK_CLIENT_NAME")
        .ok()
        .map(|product| OriginClientInfo {
            product,
            version: std::env::var("GROK_CLIENT_VERSION").ok(),
        })
}

/// Construct an [`OriginClientInfo`] from a shell-side
/// [`ClientType`] (which carries its UA label) and an optional
/// version string.
pub fn origin_client_info_from_client_type(
    client_type: ClientType,
    version: Option<String>,
) -> OriginClientInfo {
    OriginClientInfo {
        product: client_type.user_agent_label().to_string(),
        version,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PlatformInfo {
    os: String,
    arch: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct UserAgent {
    origin: OriginClientInfo,
    agent_product: &'static str,
    agent_version: String,
    platform: PlatformInfo,
}

impl PlatformInfo {
    fn current() -> Self {
        let os = match std::env::consts::OS {
            "macos" => "macos",
            "windows" => "windows",
            other => other,
        }
        .to_string();

        let arch = match std::env::consts::ARCH {
            "arm64" => "aarch64",
            "x86_64" => "x86_64",
            other => other,
        }
        .to_string();

        Self { os, arch }
    }
}

impl UserAgent {
    fn render(&self) -> String {
        if self.origin.product == self.agent_product
            && self.origin.version.as_deref() == Some(self.agent_version.as_str())
        {
            return format!(
                "{}/{} ({}; {})",
                self.agent_product, self.agent_version, self.platform.os, self.platform.arch,
            );
        }

        match self.origin.version.as_deref() {
            Some(origin_version) => format!(
                "{}/{} {}/{} ({}; {})",
                self.origin.product,
                origin_version,
                self.agent_product,
                self.agent_version,
                self.platform.os,
                self.platform.arch,
            ),
            None => format!(
                "{} {}/{} ({}; {})",
                self.origin.product,
                self.agent_product,
                self.agent_version,
                self.platform.os,
                self.platform.arch,
            ),
        }
    }
}

fn agent_version() -> String {
    xai_grok_version::VERSION.to_string()
}

/// Set the process-level fallback origin client type for `User-Agent`.
pub fn set_client_name(client_type: ClientType) {
    CLIENT_TYPE
        .set(client_type)
        .expect("set_client_name called more than once");
}

pub fn process_user_agent_string() -> String {
    let agent_version = agent_version();
    let origin = origin_client_info_from_env().unwrap_or_else(|| {
        origin_client_info_from_client_type(
            CLIENT_TYPE.get().copied().unwrap_or(ClientType::Generic),
            Some(agent_version.clone()),
        )
    });

    UserAgent {
        origin,
        agent_product: "grok-shell",
        agent_version,
        platform: PlatformInfo::current(),
    }
    .render()
}

pub fn session_user_agent_string(origin: &OriginClientInfo) -> String {
    UserAgent {
        origin: origin.clone(),
        agent_product: "grok-shell",
        agent_version: agent_version(),
        platform: PlatformInfo::current(),
    }
    .render()
}

pub fn origin_client_info_from_meta(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<OriginClientInfo> {
    let product = meta
        .and_then(|m| m.get("clientIdentifier"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            meta.and_then(|m| m.get("clientType"))
                .and_then(|v| serde_json::from_value::<ClientType>(v.clone()).ok())
                .map(|client_type| client_type.user_agent_label().to_string())
        });

    product.map(|product| OriginClientInfo {
        product,
        version: meta
            .and_then(|m| m.get("clientVersion"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

pub fn merge_origin_client_info(
    primary: Option<OriginClientInfo>,
    fallback: Option<OriginClientInfo>,
) -> Option<OriginClientInfo> {
    match (primary, fallback) {
        (Some(primary), Some(fallback)) => Some(OriginClientInfo {
            product: primary.product,
            version: primary.version.or(fallback.version),
        }),
        (Some(primary), None) => Some(primary),
        (None, Some(fallback)) => Some(fallback),
        (None, None) => None,
    }
}

pub fn client_type_from_origin(origin: Option<&OriginClientInfo>) -> ClientType {
    ClientType::from_client_identifier(origin.map(|o| o.product.as_str()))
}

/// Process-level client identifier (`GROK_CLIENT_NAME` env var, default `"grok-shell"`).
pub fn process_client_identifier() -> String {
    std::env::var("GROK_CLIENT_NAME").unwrap_or_else(|_| "grok-shell".to_string())
}

/// Header telling cli-chat-proxy whether this process is a single-prompt
/// (`grok -p`) run or an interactive session; feeds the `client_mode`
/// metric label.
pub const CLIENT_MODE_HEADER: &str = "x-grok-client-mode";

/// One-way latch: set to `"headless"` at startup by the non-TUI entry points
/// (`run_single_turn` for `grok -p`, `run_headless_inner` for
/// `grok agent [headless]`), `"interactive"` otherwise.
static CLIENT_MODE: OnceLock<&'static str> = OnceLock::new();

/// Mark this process as headless (single-prompt). No-op if already set.
pub fn set_process_client_mode_headless() {
    let _ = CLIENT_MODE.set("headless");
}

/// The mode sent in [`CLIENT_MODE_HEADER`]; defaults to `"interactive"`.
pub fn process_client_mode() -> &'static str {
    CLIENT_MODE.get().copied().unwrap_or("interactive")
}

pub fn user_agent_string_for(origin: &OriginClientInfo) -> String {
    session_user_agent_string(origin)
}

/// Returns a shared [`reqwest::Client`], creating it on first call.
///
/// The returned client is a cheap `Arc` clone — safe to pass across threads
/// and tasks. Sets a 30-second connect timeout; callers should set
/// per-request timeouts as needed.
///
/// Keeps HTTP/2 + connection pooling, but adds health-checks so a half-dead
/// pooled connection is detected and dropped instead of reused. Through an
/// LB/Cloudflare/proxy a kept-alive connection can be silently dropped upstream;
/// without these, reqwest reuses it and mints doomed streams on it, so every
/// retry fails identically and a reachable server looks unreachable. Idle/TCP
/// eviction drops connections before the upstream idle window (~60-100s; 30s is
/// a conservative default) closes them, and the HTTP/2 keepalive ping detects a
/// dead connection so the pool stops handing it out.
pub fn shared_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let _timer = startup_timer!("startup.http_client_build");
            reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(30))
                .user_agent(process_user_agent_string())
                .pool_idle_timeout(std::time::Duration::from_secs(30))
                .http2_keep_alive_interval(std::time::Duration::from_secs(20))
                .http2_keep_alive_timeout(std::time::Duration::from_secs(10))
                .http2_keep_alive_while_idle(true)
                .tcp_keepalive(std::time::Duration::from_secs(30))
                .build()
                .expect("failed to build shared HTTP client")
        })
        .clone()
}

/// Wrap a raw client with [`AuthRetryMiddleware`] for automatic 401 retry.
pub fn with_auth_retry(
    client: reqwest::Client,
    credentials: std::sync::Arc<dyn xai_grok_auth::AuthCredentialProvider>,
) -> reqwest_middleware::ClientWithMiddleware {
    reqwest_middleware::ClientBuilder::new(client)
        .with(xai_grok_auth::AuthRetryMiddleware::new(credentials, 1))
        .build()
}

/// Returns a shared [`reqwest::Client`] for GCS uploads, creating it on first call.
///
/// Unlike `shared_client()`, this client has aggressive connection pool eviction
/// to avoid reusing stale/poisoned connections during retry loops. When uploads
/// fail and trigger exponential backoff (1s, 2s, 4s...), idle connections may be
/// closed by the server, Cloudflare, or load balancers. Without pool eviction,
/// all retries would reuse the same dead connection and fail.
///
/// Settings:
/// - HTTP/1.1 only — avoids HTTP/2 connection-poisoning where a degraded
///   multiplexed connection silently drops multipart request bodies, causing
///   cascading 400 errors across all concurrent uploads
/// - Small connection pool (2 per host) for parallel chunk uploads
/// - Short idle timeout (10s) to evict stale connections before backoff completes
pub fn shared_upload_client() -> reqwest::Client {
    static UPLOAD_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    UPLOAD_CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                // Force HTTP/1.1: batch_upload multipart bodies are silently
                // dropped when an HTTP/2 connection degrades (GOAWAY, flow-control
                // exhaustion). Because all streams share one connection, a single
                // bad connection causes every subsequent request to arrive with
                // Content-Length: 0, producing thousands of 400s until the process
                // restarts. HTTP/1.1 isolates failures to individual connections.
                .http1_only()
                .pool_max_idle_per_host(2)
                .pool_idle_timeout(std::time::Duration::from_secs(10))
                .user_agent(process_user_agent_string())
                .build()
                .expect("failed to build shared upload HTTP client")
        })
        .clone()
}

/// A fresh, pool-less HTTP/1.1 [`reqwest::Client`], deliberately NOT cached:
/// `pool_max_idle_per_host(0)` + `http1_only()` so each request opens a new connection, and no
/// connect timeout (callers bound each request with their own total timeout). The retry escape
/// policy that reaches for this client to dodge a poisoned pool lives on `send_with_retry_escaping_pool`.
///
/// Fallible: build can fail under fd/TLS pressure; the caller must not
/// panic on error (fallback policy lives at the call site).
pub(crate) fn fresh_http1_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .http1_only()
        .pool_max_idle_per_host(0)
        .user_agent(process_user_agent_string())
        .build()
}

/// Joins an error's `source()` chain into one string. A `reqwest::Error`'s `Display`
/// shows only the outer "error sending request for url (...)", hiding the real hyper
/// cause (reset, closed-before-complete, timeout) reachable only via `source()`.
pub fn error_cause_chain(err: &dyn std::error::Error) -> String {
    let mut msg = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        msg.push_str(": ");
        msg.push_str(&cause.to_string());
        source = cause.source();
    }
    msg
}

/// How a `reqwest` request/send failure should be treated by a retry loop.
#[derive(Debug, PartialEq, Eq)]
pub enum TransportFailureKind {
    /// The connection could never be established (`is_connect`): the server is
    /// down or genuinely unreachable. Retrying the same target rarely helps soon.
    Unreachable,
    /// An established request was cut short — a per-request timeout, an in-flight
    /// reset/close/GOAWAY, or a body-phase drop. Retryable: a fresh connection
    /// can succeed.
    Interrupted,
    /// A client-side defect (request-builder error, redirect-policy violation):
    /// not retryable, because retrying can't fix it.
    Permanent,
}

/// A classified `reqwest` request/send failure: a [`TransportFailureKind`] plus the
/// joined cause-chain detail. Derives `PartialEq` so the kind-to-error mapping can
/// be unit-tested by constructing values directly.
#[derive(Debug, PartialEq)]
pub struct TransportFailure {
    pub kind: TransportFailureKind,
    pub detail: String,
}

impl TransportFailure {
    /// Classify a `reqwest` request/send error. `is_connect()` MUST be checked first:
    /// in reqwest 0.12 a connect failure is also `Kind::Request`.
    pub fn classify(e: &reqwest::Error) -> Self {
        let detail = error_cause_chain(e);
        let kind = if e.is_connect() {
            TransportFailureKind::Unreachable
        } else if e.is_timeout() || e.is_request() || e.is_body() {
            TransportFailureKind::Interrupted
        } else {
            TransportFailureKind::Permanent
        };
        Self { kind, detail }
    }
}

/// Run `op` with bounded retries, swapping to a fresh pool-less client for the final attempt.
///
/// NOTE: this is not a plain retry loop — it bakes in a connection-escape policy. Early attempts run
/// on the pooled [`shared_client`] (HTTP/2 + keepalive + idle/TCP eviction); the FINAL attempt of a
/// multi-attempt run instead FORCES a fresh, pool-less HTTP/1.1 client (`fresh_http1_client`) so a
/// tight-budget caller (e.g. a 2-attempt login) can escape a half-dead pooled connection without
/// waiting out the pool's own keepalive/idle eviction (~20-30s).
///
/// This only rescues a FAST-FAIL connection (reset/GOAWAY/refused) within budget: the fresh attempt
/// returns quickly and succeeds. A silently black-holed connection still burns the caller's
/// per-request timeout on each attempt, so a tight deadline can elapse first and recovery defers to
/// the background sync loop / next start (best-effort, the documented behavior).
///
/// `op` receives the client to use and returns the WHOLE operation's result (send + body read +
/// decode), so a body-phase interruption is inside the retried unit, not just the send. `is_retryable`
/// decides whether a given error earns another attempt, so the caller keeps its own typed retry policy
/// (e.g. retry 5xx, fail fast on auth). `backoff(attempt)` is awaited before attempt N (N >= 1),
/// keeping this helper runtime-agnostic (the caller supplies the sleep). The client is passed by value
/// (a cheap `Arc` clone) so each attempt's future owns it instead of borrowing across the loop.
pub async fn send_with_retry_escaping_pool<T, E, Op, OpFut, Backoff, BackoffFut>(
    op: Op,
    max_attempts: u32,
    is_retryable: impl Fn(&E) -> bool,
    backoff: Backoff,
) -> Result<T, E>
where
    E: std::fmt::Display,
    Op: Fn(reqwest::Client) -> OpFut,
    OpFut: std::future::Future<Output = Result<T, E>>,
    Backoff: Fn(u32) -> BackoffFut,
    BackoffFut: std::future::Future<Output = ()>,
{
    // `max(1)` guarantees at least one attempt runs, so `last_err` is set if the loop falls through.
    let max_attempts = max_attempts.max(1);
    let pooled = shared_client();
    // Built lazily (loads OS TLS roots, ~95ms) and only if a final escape attempt is actually reached.
    let mut fresh: Option<reqwest::Client> = None;
    let mut last_err: Option<E> = None;

    for attempt in 0..max_attempts {
        if attempt > 0 {
            backoff(attempt).await;
        }
        // Only the final attempt of a multi-attempt run escapes onto a fresh pool-less connection; a
        // single-attempt caller keeps the pooled client (there is no prior failure to escape).
        let client = if attempt > 0 && attempt + 1 == max_attempts {
            match &fresh {
                Some(c) => c.clone(),
                None => match fresh_http1_client() {
                    Ok(c) => fresh.insert(c).clone(),
                    // Can't escape the pool (e.g. fd exhaustion); a pooled
                    // final attempt still beats aborting the process.
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to build pool-escape client; final attempt stays on pooled client");
                        pooled.clone()
                    }
                },
            }
        } else {
            pooled.clone()
        };
        match op(client).await {
            Ok(value) => return Ok(value),
            Err(e) if is_retryable(&e) => {
                // Log recovered-transient failures (a connection-health path); a silent retry would hide a degrading pool.
                tracing::debug!(attempt, error = %e, "send_with_retry_escaping_pool: retrying after transient failure");
                last_err = Some(e);
            }
            Err(e) => return Err(e),
        }
    }

    Err(last_err.expect("send_with_retry_escaping_pool ran at least one attempt"))
}

/// Returns a shared [`reqwest::blocking::Client`], creating it on first call.
///
/// This avoids redundant TLS certificate loading for blocking HTTP calls
/// (e.g., model prefetching during startup). The blocking client is separate
/// from the async `shared_client()` because reqwest's blocking client creates
/// its own internal tokio runtime.
///
/// Mirrors `shared_client()`'s pool self-healing for the same reason: this client
/// is reused (settings, prefetch) and a kept-alive connection an LB/Cloudflare/proxy
/// silently drops would otherwise be handed back out, so a reachable server looks
/// unreachable. Idle/TCP eviction drops a connection before the upstream idle window
/// (~60-100s; 30s is a conservative default) closes it. The HTTP/2 keepalive-ping
/// setters that `shared_client()` uses are NOT exposed on reqwest's blocking
/// `ClientBuilder` (0.12), so only the idle/TCP-eviction half applies here.
pub fn shared_blocking_client() -> reqwest::blocking::Client {
    static BLOCKING_CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    BLOCKING_CLIENT
        .get_or_init(|| {
            let _timer = startup_timer!("startup.http_blocking_client_build");
            reqwest::blocking::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(30))
                .timeout(std::time::Duration::from_secs(30))
                .user_agent(process_user_agent_string())
                .pool_idle_timeout(std::time::Duration::from_secs(30))
                .tcp_keepalive(std::time::Duration::from_secs(30))
                .build()
                .expect("failed to build shared blocking HTTP client")
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cause-chain formatter appends each `source()` joined with ": ", so a
    /// reqwest error whose `Display` hides the hyper cause still surfaces it.
    #[test]
    fn error_cause_chain_appends_hidden_sources() {
        #[derive(Debug)]
        struct Leaf;
        impl std::fmt::Display for Leaf {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "connection closed before message completed")
            }
        }
        impl std::error::Error for Leaf {}

        #[derive(Debug)]
        struct Wrapper(Leaf);
        impl std::fmt::Display for Wrapper {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "error sending request")
            }
        }
        impl std::error::Error for Wrapper {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        assert_eq!(
            error_cause_chain(&Wrapper(Leaf)),
            "error sending request: connection closed before message completed",
            "the hidden source cause must be appended after ': '"
        );
    }

    #[test]
    fn origin_client_info_from_meta_extracts_identifier_and_version() {
        let meta = serde_json::json!({
            "clientIdentifier": "grok-desktop",
            "clientVersion": "1.2.3",
        })
        .as_object()
        .cloned()
        .unwrap();
        assert_eq!(
            origin_client_info_from_meta(Some(&meta)),
            Some(OriginClientInfo {
                product: "grok-desktop".to_string(),
                version: Some("1.2.3".to_string()),
            })
        );
    }

    #[test]
    fn origin_client_info_from_meta_uses_client_type_when_identifier_absent() {
        let meta = serde_json::json!({
            "clientType": "grok_pager",
            "clientVersion": "0.1.2",
        })
        .as_object()
        .cloned()
        .unwrap();
        assert_eq!(
            origin_client_info_from_meta(Some(&meta)),
            Some(OriginClientInfo {
                product: "grok-pager".to_string(),
                version: Some("0.1.2".to_string()),
            })
        );
    }

    #[test]
    fn merge_origin_client_info_preserves_primary_product_and_backfills_version() {
        let merged = merge_origin_client_info(
            Some(OriginClientInfo {
                product: "grok-web".to_string(),
                version: None,
            }),
            Some(OriginClientInfo {
                product: "grok-desktop".to_string(),
                version: Some("1.2.3".to_string()),
            }),
        );
        assert_eq!(
            merged,
            Some(OriginClientInfo {
                product: "grok-web".to_string(),
                version: Some("1.2.3".to_string()),
            })
        );
    }

    #[test]
    fn session_user_agent_string_renders_expected_variants() {
        let with_version = session_user_agent_string(&OriginClientInfo {
            product: "grok-desktop".to_string(),
            version: Some("1.2.3".to_string()),
        });
        assert!(with_version.starts_with("grok-desktop/1.2.3 grok-shell/"));
        assert!(with_version.contains(" ("));

        let without_version = session_user_agent_string(&OriginClientInfo {
            product: "grok-web".to_string(),
            version: None,
        });
        assert!(without_version.starts_with("grok-web grok-shell/"));
        assert!(!without_version.starts_with("grok-web/"));
    }

    #[test]
    fn user_agent_render_collapses_duplicate_origin_and_agent_identity() {
        let ua = UserAgent {
            origin: OriginClientInfo {
                product: "grok-shell".to_string(),
                version: Some("0.1.171".to_string()),
            },
            agent_product: "grok-shell",
            agent_version: "0.1.171".to_string(),
            platform: PlatformInfo {
                os: "macos".to_string(),
                arch: "aarch64".to_string(),
            },
        };

        assert_eq!(ua.render(), "grok-shell/0.1.171 (macos; aarch64)");
    }
}
