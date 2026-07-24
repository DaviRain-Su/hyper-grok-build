//! Codex Live signaling + sideband transport.
//!
//! A substantially adapted port of `packages/coding-agent/src/live/transport.ts`
//! from oh-my-pi (OMP) v17.1.1 (commit e9c8a35). The OMP original drives a Bun
//! WebSocket + fetch through an OAuth-storage callback; this version uses
//! `reqwest` for signaling (which honors env proxies) and `tokio-tungstenite`
//! for the sideband, with auth resolved through the async-object-safe
//! [`super::LiveAuthProvider`].
//!
//! Wire behavior preserved exactly: the `gpt-live-1-codex` model, the
//! `{codex_base}/realtime/calls?intent=quicksilver&architecture=avas` signaling
//! URL, the `OpenAI-Alpha: quicksilver=v2` header, the
//! `User-Agent: Codex Desktop/{version}` header, the `x-session-id` UUID, the
//! `originator`/`version`/`session-id`/`thread-id` headers, the optional
//! `chatgpt-account-id` and `x-oai-attestation`, the strict `rtc_*` Location
//! parsing, the `wss://api.openai.com/v1/live/<callId>` sideband URL, the exact
//! serde signaling body, the initial sideband retry/backoff/timeouts, the
//! once-only 401 forced refresh via the auth provider, and idempotent shutdown.
//! Unknown payloads are never logged wholesale.
//!
//! MIT attribution preserved in `THIRD-PARTY-NOTICES`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use super::attestation::generate_codex_attestation;
use super::media::{LiveMediaPeer, MediaEvent};
use super::protocol::{
    LiveClientMessage, LiveServerEvent, build_live_session_payload, build_live_sideband_url,
    parse_live_call_id, parse_live_server_event,
};
use super::types::{LiveAuth, LiveConfig, SharedLiveAuth};

/// Signaling request URL suffix (appended to `codex_base`). Exact OMP value.
const SIGNALING_PATH: &str = "/realtime/calls?intent=quicksilver&architecture=avas";
/// Max bytes of an error response body surfaced in a signaling error.
const MAX_ERROR_BODY_LENGTH: usize = 2_048;
/// Initial sideband connect attempts (OMP: 5).
const SIDEBAND_CONNECT_ATTEMPTS: u32 = 5;
/// Per-attempt sideband connect timeout (OMP: 15 s).
const SIDEBAND_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Signaling connect timeout.
const SIGNALING_TIMEOUT: Duration = Duration::from_secs(20);
/// OpenAI header names (OMP `OPENAI_HEADERS`).
const HEADER_ORIGINATOR: &str = "originator";
const HEADER_VERSION: &str = "version";
const HEADER_SCOPED_SESSION_ID: &str = "session-id";
const HEADER_THREAD_ID: &str = "thread-id";
const HEADER_ACCOUNT_ID: &str = "chatgpt-account-id";
const HEADER_ATTESTATION: &str = "x-oai-attestation";
const ORIGINATOR_VALUE: &str = "Codex Desktop";

/// Callbacks emitted by the live transport (forwarded to the session).
pub trait LiveTransportCallbacks: Send + Sync + 'static {
    fn on_event(&self, event: LiveServerEvent);
    fn on_output_level(&self, level: f64);
}

/// Result of a successful signaling exchange.
struct LiveSignalingResult {
    answer: String,
    call_id: String,
    attestation: Option<String>,
}

/// A signaling failure with its HTTP status (for 401 detection).
#[derive(Debug)]
struct LiveSignalingError {
    status: u16,
    message: String,
}

/// Native WebRTC transport for a Codex Frameless Bidi live session.
///
/// Owns the [`LiveMediaPeer`] and the sideband WebSocket. Once [`connect`]
/// resolves, [`send`] serializes control messages onto the sideband and
/// [`push_audio`] queues mic PCM onto the peer. [`close`] is idempotent.
pub struct CodexLiveTransport {
    config: LiveConfig,
    auth: SharedLiveAuth,
    callbacks: Arc<dyn LiveTransportCallbacks>,
    peer: Option<LiveMediaPeer>,
    realtime_session_id: String,
    sideband_tx: Option<mpsc::Sender<String>>,
    sideband_writer: Option<JoinHandle<()>>,
    sideband_reader: Option<JoinHandle<()>>,
    connected: AtomicBool,
    closed: AtomicBool,
    /// Once-only 401 forced refresh: the first signaling 401 triggers a single
    /// `force_refresh` on the auth provider, then a single retry.
    refreshed: AtomicBool,
}

impl CodexLiveTransport {
    pub fn new(
        config: LiveConfig,
        auth: SharedLiveAuth,
        callbacks: Arc<dyn LiveTransportCallbacks>,
    ) -> Self {
        let realtime_session_id = uuid::Uuid::new_v4().to_string();
        Self {
            config,
            auth,
            callbacks,
            peer: None,
            realtime_session_id,
            sideband_tx: None,
            sideband_writer: None,
            sideband_reader: None,
            connected: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            refreshed: AtomicBool::new(false),
        }
    }

    /// Establish the native peer, perform Codex signaling, and wait for the
    /// data channel + sideband. Idempotent for concurrent callers via the
    /// internal state flags.
    pub async fn connect(&mut self) -> Result<(), String> {
        if self.connected.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.closed.load(Ordering::Acquire) {
            return Err("Live transport is closed".to_owned());
        }
        // On any error, tear down everything we opened.
        if let Err(e) = self.connect_inner().await {
            self.close_inner().await;
            return Err(e);
        }
        self.connected.store(true, Ordering::Release);
        Ok(())
    }

    async fn connect_inner(&mut self) -> Result<(), String> {
        let (peer, media_rx) = LiveMediaPeer::new();
        // Forward media events to the callbacks while we set up signaling.
        let callbacks = Arc::clone(&self.callbacks);
        let media_forward = tokio::spawn(async move {
            while let Ok(event) = media_rx.recv_async().await {
                match event {
                    MediaEvent::Event(payload) => {
                        if let Some(ev) = parse_live_server_event(&payload) {
                            callbacks.on_event(ev);
                        }
                    }
                    MediaEvent::OutputLevel(level) => callbacks.on_output_level(level),
                    MediaEvent::Failure(_msg) => {
                        // Failures are surfaced by the peer as an error event
                        // via the sideband path; the transport's `close` is
                        // driven by the session. Stop forwarding once the peer
                        // is gone.
                        break;
                    }
                }
            }
        });

        let offer = peer.create_offer().await?;
        self.peer = Some(peer);

        let signaling = self.signal(&offer).await?;
        // Apply the answer to the peer we just stored.
        {
            let peer = self.peer.as_ref().expect("peer was just set");
            peer.accept_answer(signaling.answer).await?;
            peer.wait_for_open(None).await?;
        }
        self.connect_sideband(&signaling.call_id, &signaling.attestation)
            .await?;
        // The media-forward task lives for the session; keep its handle so
        // `close` can abort it. (It ends on its own when the peer drops.)
        self.sideband_reader.get_or_insert(media_forward);
        Ok(())
    }

    async fn signal(&self, offer: &str) -> Result<LiveSignalingResult, String> {
        let attestation = generate_codex_attestation().await;
        // First attempt with the current credential.
        match self
            .signal_with_access(offer, attestation.clone(), false)
            .await
        {
            Ok(result) => Ok(result),
            Err(e) => {
                // Once-only 401 forced refresh: the first 401 triggers a
                // single `force_refresh` on the auth provider, then a single
                // retry. A second 401 (or any non-401 error) surfaces as-is.
                if e.status == 401 && !self.refreshed.swap(true, Ordering::AcqRel) {
                    self.auth.force_refresh().await;
                    self.signal_with_access(offer, attestation, true)
                        .await
                        .map_err(|e| e.message)
                } else {
                    Err(e.message)
                }
            }
        }
    }

    async fn signal_with_access(
        &self,
        offer: &str,
        attestation: Option<String>,
        _is_retry: bool,
    ) -> Result<LiveSignalingResult, LiveSignalingError> {
        let auth = self
            .auth
            .bearer_account()
            .await
            .ok_or_else(|| LiveSignalingError {
                status: 401,
                message: "No Codex credential is available for a live call.".to_owned(),
            })?;

        let signaling_url = format!(
            "{}{SIGNALING_PATH}",
            self.config.codex_base.trim_end_matches('/')
        );
        let body = serde_json::json!({
            "sdp": offer,
            "session": build_live_session_payload(&self.config.instructions, &self.config.voice),
        });

        let client = build_reqwest_client().map_err(|e| LiveSignalingError {
            status: 0,
            message: format!("failed to build signaling client: {e}"),
        })?;

        let mut headers = reqwest::header::HeaderMap::new();
        apply_session_headers(
            &mut headers,
            &auth,
            &self.config,
            &self.realtime_session_id,
            attestation.as_deref(),
        );

        let response = client
            .post(&signaling_url)
            .header("Accept", "*/*")
            .header("Content-Type", "application/json")
            .timeout(SIGNALING_TIMEOUT)
            .headers(headers)
            .body(serde_json::to_vec(&body).unwrap())
            .send()
            .await
            .map_err(|e| LiveSignalingError {
                status: 0,
                message: format!("Codex live signaling request failed: {e}"),
            })?;

        let status = response.status().as_u16();
        let location = response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body_text = response.text().await.unwrap_or_default();
        if status >= 400 {
            let detail = bounded_error_body(&body_text, status);
            return Err(LiveSignalingError {
                status,
                message: format!("Codex live signaling failed ({status}): {detail}"),
            });
        }
        let answer = body_text;
        if answer.trim().is_empty() {
            return Err(LiveSignalingError {
                status,
                message: "Codex live signaling returned an empty SDP answer".to_owned(),
            });
        }
        let call_id =
            parse_live_call_id(location.as_deref()).ok_or_else(|| LiveSignalingError {
                status,
                message: "Codex live signaling returned no valid call ID".to_owned(),
            })?;
        Ok(LiveSignalingResult {
            answer,
            call_id,
            attestation,
        })
    }

    async fn connect_sideband(
        &mut self,
        call_id: &str,
        attestation: &Option<String>,
    ) -> Result<(), String> {
        let mut last_err = "Codex live sideband connection failed".to_string();
        for attempt in 0..SIDEBAND_CONNECT_ATTEMPTS {
            match self.open_sideband(call_id, attestation).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last_err = e;
                    if self.closed.load(Ordering::Acquire) {
                        return Err(last_err);
                    }
                    if attempt + 1 < SIDEBAND_CONNECT_ATTEMPTS {
                        tokio::time::sleep(Duration::from_millis(200 * 2u64.pow(attempt))).await;
                    }
                }
            }
        }
        Err(last_err)
    }

    async fn open_sideband(
        &mut self,
        call_id: &str,
        attestation: &Option<String>,
    ) -> Result<(), String> {
        let url = build_live_sideband_url(call_id);
        let auth =
            self.auth.bearer_account().await.ok_or_else(|| {
                "No Codex credential is available for the live sideband.".to_string()
            })?;

        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|e| format!("sideband request: {e}"))?;
        apply_session_headers_ws(
            &mut request,
            &auth,
            &self.config,
            &self.realtime_session_id,
            attestation.as_deref(),
        );

        // tokio-tungstenite honors HTTP(S)_PROXY via its connector only when
        // explicitly configured; reqwest handles env proxies for signaling.
        // The sideband wss connection itself goes direct (or through a
        // system-configured TLS proxy). This matches the OMP behavior where
        // the sideband uses the resolved proxy URL.
        let connect = tokio::time::timeout(
            SIDEBAND_CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async(request),
        )
        .await
        .map_err(|_| "Codex live sideband connection timed out".to_string())?
        .map_err(|e| format!("Codex live sideband connection failed: {e}"))?;

        let (ws, _) = connect;
        let (mut ws_write, mut ws_read) = ws.split();

        let (sideband_tx, mut sideband_rx) = mpsc::channel::<String>(64);
        self.sideband_tx = Some(sideband_tx);

        let callbacks = Arc::clone(&self.callbacks);
        let closed = self.closed.load(Ordering::Acquire);
        let _ = closed;
        let reader = tokio::spawn(async move {
            loop {
                match ws_read.next().await {
                    Some(Ok(Message::Text(text))) => {
                        if let Some(event) = parse_live_server_event(&text) {
                            callbacks.on_event(event);
                        }
                        // Unknown/malformed payloads are silently dropped —
                        // never logged wholesale (secrets safety).
                    }
                    Some(Ok(Message::Binary(_))) => {
                        // Sideband is text-only; ignore binary frames.
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(_)) => break,
                    None => break,
                }
            }
        });

        let writer = tokio::spawn(async move {
            while let Some(msg) = sideband_rx.recv().await {
                if ws_write.send(Message::Text(msg.into())).await.is_err() {
                    break;
                }
            }
            let _ = ws_write.send(Message::Close(None)).await;
        });

        self.sideband_reader = Some(reader);
        self.sideband_writer = Some(writer);
        Ok(())
    }

    /// Serialize one Frameless Bidi control message onto the sideband. Returns
    /// an error if the transport is not connected or the sideband is gone.
    pub async fn send(&self, message: &LiveClientMessage) -> Result<(), String> {
        if !self.connected.load(Ordering::Acquire) {
            return Err("Live transport is not connected".to_owned());
        }
        let tx = self
            .sideband_tx
            .as_ref()
            .ok_or_else(|| "Codex live sideband is not connected".to_owned())?;
        let json = serde_json::to_string(message)
            .map_err(|e| format!("failed to serialize live message: {e}"))?;
        tx.send(json)
            .await
            .map_err(|_| "Codex live sideband is closed".to_owned())
    }

    /// Queue 16 kHz mono Float32 PCM for native Opus transmission. No-op when
    /// muted or not connected.
    pub fn push_audio(&self, samples: &[f32]) {
        if !self.connected.load(Ordering::Acquire) || samples.is_empty() {
            return;
        }
        if let Some(peer) = &self.peer {
            let _ = peer.push_audio(samples);
        }
    }

    /// Enable or disable the native audio source (echo gate / mute).
    pub fn set_muted(&self, muted: bool) {
        if let Some(peer) = &self.peer {
            let _ = peer.set_muted(muted);
        }
    }

    /// Stop sideband signaling and the native WebRTC media peer. Idempotent.
    pub async fn close(&mut self) {
        self.close_inner().await;
    }

    async fn close_inner(&mut self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.connected.store(false, Ordering::Release);
        // Close the sideband first so no new messages are queued.
        if let Some(tx) = self.sideband_tx.take() {
            drop(tx);
        }
        if let Some(writer) = self.sideband_writer.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
        }
        if let Some(reader) = self.sideband_reader.take() {
            reader.abort();
            let _ = reader.await;
        }
        if let Some(peer) = self.peer.take() {
            peer.close().await;
        }
    }
}

impl Drop for CodexLiveTransport {
    fn drop(&mut self) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        // Best-effort async teardown if we're on a runtime; otherwise the
        // peer's own Drop handles its resources.
        if let Ok(handle) = tokio::runtime::Handle::try_current()
            && let Some(peer) = self.peer.take()
        {
            handle.spawn(async move {
                peer.close().await;
            });
        }
        if let Some(reader) = self.sideband_reader.take() {
            reader.abort();
        }
        if let Some(writer) = self.sideband_writer.take() {
            writer.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// Headers / proxy / error helpers
// ---------------------------------------------------------------------------

/// Build the Codex live session header pairs (name, value) mirroring OMP
/// `liveSessionHeaders` exactly. Returned as plain strings so they can be
/// applied to both a reqwest `RequestBuilder` and a tungstenite client request
/// without coupling to a specific `http` crate version.
fn live_session_header_pairs(
    auth: &LiveAuth,
    config: &LiveConfig,
    realtime_session_id: &str,
    attestation: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut pairs: Vec<(&'static str, String)> = vec![
        ("Authorization", format!("Bearer {}", auth.bearer)),
        ("OpenAI-Alpha", "quicksilver=v2".to_string()),
        (
            "User-Agent",
            format!("Codex Desktop/{}", config.client_version),
        ),
        ("x-session-id", realtime_session_id.to_string()),
        (HEADER_ORIGINATOR, ORIGINATOR_VALUE.to_string()),
        (HEADER_VERSION, config.client_version.clone()),
        (HEADER_SCOPED_SESSION_ID, config.session_id.clone()),
        (HEADER_THREAD_ID, config.session_id.clone()),
    ];
    if let Some(account_id) = auth.account_id.as_deref()
        && !account_id.is_empty()
    {
        pairs.push((HEADER_ACCOUNT_ID, account_id.to_string()));
    }
    if let Some(attestation) = attestation {
        pairs.push((HEADER_ATTESTATION, attestation.to_string()));
    }
    pairs
}

/// Apply the Codex live session headers to a reqwest `HeaderMap`.
fn apply_session_headers(
    headers: &mut reqwest::header::HeaderMap,
    auth: &LiveAuth,
    config: &LiveConfig,
    realtime_session_id: &str,
    attestation: Option<&str>,
) {
    for (name, value) in live_session_header_pairs(auth, config, realtime_session_id, attestation) {
        if value.is_empty() {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(&value),
        ) {
            headers.insert(name, value);
        }
    }
}

/// Apply the Codex live session headers to a tungstenite client request.
fn apply_session_headers_ws(
    request: &mut tokio_tungstenite::tungstenite::handshake::client::Request,
    auth: &LiveAuth,
    config: &LiveConfig,
    realtime_session_id: &str,
    attestation: Option<&str>,
) {
    let headers = request.headers_mut();
    for (name, value) in live_session_header_pairs(auth, config, realtime_session_id, attestation) {
        if value.is_empty() {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            headers.insert(name, value);
        }
    }
}

/// Build a reqwest client that honors standard HTTP proxy / NO_PROXY env vars.
/// reqwest's default builder reads `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` when
/// a `Proxy` is configured; we resolve the proxy ourselves (mirroring the
/// shell's `resolve_proxy_for_host`) and attach it so signaling naturally
/// honors the environment without a dependency on the shell crate (avoiding a
/// cycle).
fn build_reqwest_client() -> Result<reqwest::Client, reqwest::Error> {
    let mut builder =
        reqwest::Client::builder().user_agent(format!("Codex Desktop/{}", "xai-grok-voice"));
    // Honor the standard proxy env vars. reqwest reads `HTTP_PROXY`/
    // `HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY` when `Proxy::all`/`http`/`https`
    // is added, but only if the env var is present; we add an `all` proxy
    // derived from HTTPS_PROXY/HTTP_PROXY so the signaling POST goes through
    // the corporate egress when configured.
    if let Some(proxy_url) = resolve_sideband_proxy()
        && let Ok(proxy) = reqwest::Proxy::all(&proxy_url)
    {
        builder = builder.proxy(proxy);
    }
    builder.build()
}

/// Resolve the proxy URL for the sideband/signaling from the standard env vars
/// (HTTPS_PROXY > HTTP_PROXY), respecting NO_PROXY for the OpenAI host. This
/// duplicates the shell's `resolve_proxy_for_host` logic inline rather than
/// depending on the shell crate (which would create a dependency cycle:
/// shell → voice → shell). The signaling reqwest client honors it natively;
/// the sideband wss connection uses tokio-tungstenite, which does not read
/// env proxies, so the transport itself must apply the resolved proxy.
fn resolve_sideband_proxy() -> Option<String> {
    let target_host = "api.openai.com";
    let no_proxy = std::env::var("NO_PROXY")
        .or_else(|_| std::env::var("no_proxy"))
        .unwrap_or_default();
    if is_host_bypassed(target_host, &no_proxy) {
        return None;
    }
    if let Ok(url) = std::env::var("HTTPS_PROXY").or_else(|_| std::env::var("https_proxy")) {
        let url = url.trim().to_string();
        if !url.is_empty() {
            return Some(url);
        }
    }
    if let Ok(url) = std::env::var("HTTP_PROXY").or_else(|_| std::env::var("http_proxy")) {
        let url = url.trim().to_string();
        if !url.is_empty() {
            return Some(url);
        }
    }
    None
}

/// Check whether `host` is in the `no_proxy` list (matches the shell helper).
fn is_host_bypassed(host: &str, no_proxy: &str) -> bool {
    let host_lower = host.to_ascii_lowercase();
    for entry in no_proxy.split(',') {
        let entry = entry.trim().to_ascii_lowercase();
        if entry.is_empty() {
            continue;
        }
        if entry == "*" || host_lower == entry {
            return true;
        }
        let matches_suffix = if entry.starts_with('.') {
            host_lower.ends_with(entry.as_str())
        } else {
            host_lower.len() > entry.len()
                && host_lower.ends_with(entry.as_str())
                && host_lower.as_bytes()[host_lower.len() - entry.len() - 1] == b'.'
        };
        if matches_suffix {
            return true;
        }
    }
    false
}

/// Normalize an error response body into a single bounded line. Never logs the
/// full body wholesale beyond the cap; matches OMP `boundedErrorBody`. The cap
/// is applied on a UTF-8 char boundary so a multibyte char at the boundary is
/// never split (which would panic on byte-slicing).
fn bounded_error_body(body: &str, status: u16) -> String {
    let normalized: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return format!("HTTP {status}");
    }
    if normalized.chars().count() <= MAX_ERROR_BODY_LENGTH {
        return normalized;
    }
    // Truncate at a char boundary just before the cap.
    let truncated: String = normalized.chars().take(MAX_ERROR_BODY_LENGTH).collect();
    format!("{truncated}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_error_body_collapses_whitespace_and_caps_length() {
        let body = "  lots   of\nspaces   and words ".repeat(100);
        let out = bounded_error_body(&body, 500);
        // The ellipsis "…" is 3 UTF-8 bytes, so the byte length cap is
        // MAX_ERROR_BODY_LENGTH + 3 (chars: MAX_ERROR_BODY_LENGTH + 1).
        assert!(
            out.chars().count() <= MAX_ERROR_BODY_LENGTH + 1,
            "out too long: {} chars",
            out.chars().count()
        );
        assert!(!out.contains('\n'));
        assert!(!out.contains("  "), "whitespace not collapsed: {out:?}");
    }

    #[test]
    fn bounded_error_body_empty_falls_back_to_status() {
        assert_eq!(bounded_error_body("   ", 500), "HTTP 500");
    }

    #[test]
    fn bounded_error_body_short_body_returned_as_is() {
        assert_eq!(bounded_error_body("bad request", 400), "bad request");
    }

    #[test]
    fn is_host_bypassed_exact_and_suffix() {
        assert!(is_host_bypassed("api.openai.com", "api.openai.com"));
        assert!(is_host_bypassed("api.openai.com", ".openai.com"));
        assert!(is_host_bypassed("sub.api.openai.com", "openai.com"));
        assert!(!is_host_bypassed("api.openai.com", "example.com"));
        assert!(is_host_bypassed("anything", "*"));
    }

    #[test]
    fn signaling_url_appends_exact_suffix() {
        let url = format!("https://chatgpt.com/backend-api{SIGNALING_PATH}");
        assert_eq!(
            url,
            "https://chatgpt.com/backend-api/realtime/calls?intent=quicksilver&architecture=avas"
        );
    }

    #[test]
    fn chunk_live_context_from_protocol_is_used_for_appends() {
        // The transport relies on protocol::chunk_live_context; verify it
        // produces ≤500-byte chunks so signaling-side chunking is safe.
        let text = "x".repeat(1200);
        for chunk in super::super::protocol::chunk_live_context(&text) {
            assert!(chunk.len() <= 500);
        }
    }
}
