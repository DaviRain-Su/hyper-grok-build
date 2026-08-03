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
///
/// `extra_ca_pem_files` are PEM bundle paths whose certificates are added to
/// the trusted roots (the external stream's `OTEL_EXPORTER_OTLP_CERTIFICATE`,
/// for customer collectors behind a private CA). Errors reading or parsing a
/// listed bundle fail construction — exporting without a CA the user
/// explicitly configured would silently verify against the wrong trust set.
pub(crate) fn build_blocking_client(
    timeout: std::time::Duration,
    extra_ca_pem_files: &[&str],
) -> Result<BlockingOtlpClient, String> {
    let mut extra_roots = Vec::new();
    for path in extra_ca_pem_files {
        let pem = std::fs::read(path)
            .map_err(|e| format!("reading OTEL_EXPORTER_OTLP_CERTIFICATE {path:?}: {e}"))?;
        let certs = reqwest::Certificate::from_pem_bundle(&pem)
            .map_err(|e| format!("parsing OTEL_EXPORTER_OTLP_CERTIFICATE {path:?}: {e}"))?;
        // A readable but certificate-less bundle must fail closed too:
        // building a client that verifies without the configured CA would
        // silently use the wrong trust set.
        if certs.is_empty() {
            return Err(format!(
                "OTEL_EXPORTER_OTLP_CERTIFICATE {path:?} contains no certificates"
            ));
        }
        extra_roots.extend(certs);
    }
    std::thread::Builder::new()
        .name("otlp-client-build".into())
        .spawn(move || {
            // Two additive trust sources on top of the embedded webpki
            // roots: the process-wide `GROK_EXTRA_CA_BUNDLE` (fail-open,
            // handled inside xai-grok-extra-ca) and the external stream's
            // per-call `OTEL_EXPORTER_OTLP_CERTIFICATE` files (fail-closed,
            // validated above).
            let mut builder = xai_grok_extra_ca::with_extra_root_certificates_blocking(
                reqwest::blocking::Client::builder().timeout(timeout),
            );
            for cert in extra_roots {
                builder = builder.add_root_certificate(cert);
            }
            builder
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
        build_blocking_client(std::time::Duration::from_secs(5), &[])
            .expect("client with embedded webpki roots must build on any host");
    }

    /// A configured-but-unreadable customer CA must fail construction (the
    /// caller degrades by disabling the stream) instead of silently building
    /// a client that verifies against the wrong trust set.
    #[test]
    fn blocking_otlp_client_fails_closed_on_missing_ca_file() {
        let err = build_blocking_client(
            std::time::Duration::from_secs(5),
            &["/nonexistent/corp-ca.pem"],
        )
        .expect_err("missing CA bundle must fail construction");
        assert!(err.contains("OTEL_EXPORTER_OTLP_CERTIFICATE"), "{err}");
    }

    /// A readable but certificate-less bundle must also fail closed instead
    /// of building a client that verifies against the default roots only.
    #[test]
    fn blocking_otlp_client_fails_closed_on_empty_ca_bundle() {
        let file = tempfile::NamedTempFile::new().expect("temp CA file");
        std::fs::write(file.path(), "# readable, but no PEM certificate blocks\n")
            .expect("write empty bundle");
        let err = build_blocking_client(
            std::time::Duration::from_secs(5),
            &[file.path().to_str().expect("utf-8 path")],
        )
        .expect_err("certificate-less bundle must fail construction");
        assert!(err.contains("no certificates"), "{err}");
    }
}
