//! Shared construction of the blocking `reqwest` client used by the OTLP
//! HTTP exporters (spans in `otel_layer`, logs/metrics in `external`).
//!
//! This uses the workspace `reqwest` 0.12 (`rustls-tls`, embedded webpki
//! roots) rather than reqwest 0.13. reqwest 0.13's blocking client runs its
//! rustls/aws-lc-rs handshake on the fixed-stack, un-sizable
//! `reqwest-internal-sync-runtime` thread; that handshake overflows the stack
//! on the first OTLP export and crashes the CLI a few seconds after launch
//! (observed on Windows arm64; `RUST_MIN_STACK` does not help because reqwest
//! owns that thread). reqwest 0.12 shares the known-good TLS stack the rest of
//! the CLI already uses, and its embedded roots keep the exporter working on
//! hosts with no system CA store. `opentelemetry-http` only ships an
//! `HttpClient` impl for its pinned reqwest 0.13, so the 0.12 client is wrapped
//! below (orphan rule). Construction returns an error for callers to degrade on
//! (disable the exporter, keep the session alive) instead of panicking.

use async_trait::async_trait;
use bytes::Bytes;
use opentelemetry_http::{HttpClient, HttpError};

/// `opentelemetry_http::HttpClient` over the workspace reqwest 0.12 blocking
/// client. Mirrors `opentelemetry-http`'s built-in reqwest 0.13 blocking impl.
#[derive(Debug, Clone)]
pub(crate) struct BlockingOtlpClient(reqwest::blocking::Client);

#[async_trait]
impl HttpClient for BlockingOtlpClient {
    async fn send_bytes(
        &self,
        request: http::Request<Bytes>,
    ) -> Result<http::Response<Bytes>, HttpError> {
        let request = request.try_into()?;
        let mut response = self.0.execute(request)?.error_for_status()?;
        let headers = std::mem::take(response.headers_mut());
        let mut http_response = http::Response::builder()
            .status(response.status())
            .body(response.bytes()?)?;
        *http_response.headers_mut() = headers;
        Ok(http_response)
    }
}

/// Build the blocking OTLP HTTP client on a dedicated thread.
///
/// The blocking client can't be built inside a Tokio runtime, and the batch
/// processors drive exports from non-Tokio threads — building on a fresh
/// thread avoids the "no reactor" panic for every caller.
pub(crate) fn build_blocking_client(
    timeout: std::time::Duration,
) -> Result<BlockingOtlpClient, String> {
    std::thread::Builder::new()
        .name("otlp-client-build".into())
        .spawn(move || {
            xai_grok_extra_ca::with_extra_root_certificates_blocking(
                reqwest::blocking::Client::builder().timeout(timeout),
            )
            .build()
            .map(BlockingOtlpClient)
            .map_err(|e| format!("building blocking OTLP HTTP client: {e}"))
        })
        .map_err(|e| format!("spawning OTLP client builder thread: {e}"))?
        .join()
        .map_err(|_| "OTLP client builder thread panicked".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The client must build without consulting the system CA store — reqwest
    /// 0.12 `rustls-tls` trusts embedded webpki roots, so this holds on hosts
    /// with no system CA store.
    #[test]
    fn blocking_otlp_client_builds_with_embedded_roots() {
        build_blocking_client(std::time::Duration::from_secs(5))
            .expect("client with embedded webpki roots must build on any host");
    }
}
