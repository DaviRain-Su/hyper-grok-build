//! Codex Live signaling + sideband transport.
//!
//! A substantially adapted port of `packages/coding-agent/src/live/transport.ts`
//! from oh-my-pi (OMP) v17.1.1 (commit e9c8a35). The OMP original drives a Bun
//! WebSocket + fetch through an OAuth-storage callback; this version uses
//! `reqwest` for signaling (which honors env proxies) and `tokio-tungstenite`
//! for the sideband, with auth resolved through the async-object-safe
//! [`super::LiveAuthProvider`].
//!
//! Application wire behavior preserved exactly: the `gpt-live-1-codex` model,
//! the `{codex_base}/realtime/calls?intent=quicksilver&architecture=avas`
//! signaling URL, the `OpenAI-Alpha: quicksilver=v2` header, the `User-Agent:
//! Codex Desktop/{version}` header, the `x-session-id` UUID, the
//! `originator`/`version`/`session-id`/`thread-id` headers, the optional
//! `chatgpt-account-id` and `x-oai-attestation`, the strict `rtc_*` Location
//! parsing, the `wss://api.openai.com/v1/live/<callId>` sideband URL, the exact
//! serde signaling body, the initial sideband retry/backoff/timeouts, the
//! once-only 401 forced refresh via the auth provider, and idempotent shutdown.
//! Hyper additionally sends transport-level WebSocket Ping frames during quiet
//! periods so intermediaries do not reap a healthy sideband. Unknown payloads
//! are never logged wholesale.
//!
//! MIT attribution preserved in `THIRD-PARTY-NOTICES`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, client_async};

use super::attestation::generate_codex_attestation;
use super::media::{LiveMediaPeer, MediaEvent};
use super::protocol::{
    LiveClientMessage, LiveServerEvent, build_live_session_payload,
    build_live_sideband_url_with_base, parse_live_call_id, parse_live_server_event,
};
use super::types::{LiveAuth, LiveConfig, SharedLiveAuth};

/// Signaling request suffix appended to Hyper's Codex platform base, which
/// already ends in `/codex`. The combined URL exactly matches OMP's
/// `{CODEX_BASE_URL}/codex/realtime/calls?...` wire endpoint.
const SIGNALING_PATH: &str = "/realtime/calls?intent=quicksilver&architecture=avas";
/// Max bytes of an error response body surfaced in a signaling error.
const MAX_ERROR_BODY_LENGTH: usize = 2_048;
/// Initial sideband connect attempts (OMP: 5).
const SIDEBAND_CONNECT_ATTEMPTS: u32 = 5;
/// Per-attempt sideband connect timeout (OMP: 15 s).
const SIDEBAND_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Keep the control WebSocket active through proxies/load balancers that reap
/// otherwise-silent connections after roughly 60–100 seconds. WebRTC media is
/// a separate transport and does not keep the sideband WebSocket alive.
const SIDEBAND_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);
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

/// Build the exact OMP signaling URL from the Codex platform base.
fn live_signaling_url(codex_base: &str) -> String {
    format!("{}{SIGNALING_PATH}", codex_base.trim_end_matches('/'))
}

/// Preserve the WebSocket close code/reason in the user-facing terminal error,
/// matching OMP's `sideband closed (<code>): <reason>` diagnostic.
fn sideband_close_message(frame: Option<&CloseFrame>) -> String {
    let Some(frame) = frame else {
        return "Codex live sideband closed without a close frame".to_owned();
    };
    let reason = frame
        .reason
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if reason.is_empty() {
        format!("Codex live sideband closed ({})", frame.code)
    } else {
        format!("Codex live sideband closed ({}): {reason}", frame.code)
    }
}

/// Drain application control messages and emit protocol-level keepalive pings
/// while the application is idle. The Ping frame carries no conversation data;
/// it only keeps the sideband path alive through idle-reaping intermediaries.
async fn run_sideband_writer<S>(
    mut ws_write: S,
    mut sideband_rx: mpsc::Receiver<String>,
    keepalive_interval: Duration,
) where
    S: futures_util::Sink<Message> + Unpin,
{
    let first_keepalive = tokio::time::Instant::now() + keepalive_interval;
    let mut keepalive = tokio::time::interval_at(first_keepalive, keepalive_interval);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let message = tokio::select! {
            biased;
            message = sideband_rx.recv() => match message {
                Some(message) => Message::Text(message.into()),
                None => break,
            },
            _ = keepalive.tick() => Message::Ping(Vec::new().into()),
        };
        if ws_write.send(message).await.is_err() {
            break;
        }
    }
    let _ = ws_write.send(Message::Close(None)).await;
}

/// Decide whether a parsed data-channel (`oai-events`) server event should be
/// forwarded to the callbacks, given the current sideband-open state.
///
/// Matches OMP `transport.ts` `#handleServerEvent(payload)`: when the sideband
/// WebSocket is OPEN the sideband owns server events, so **every** data-channel
/// event is ignored **except `error`** (which is always surfaced). Before the
/// sideband opens (or after it closes) the data-channel is the fallback path
/// and its events are accepted.
///
/// This is a pure routing predicate so it can be unit-tested independently of
/// the async media-forward task.
fn route_data_channel_event(sideband_open: bool, event: &LiveServerEvent) -> bool {
    if !sideband_open {
        return true;
    }
    // Sideband owns server events once open; only `error` is still accepted
    // from the data channel (OMP: `except error`).
    matches!(event, LiveServerEvent::Error { .. })
}

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
    /// Media-event forward task (peer signal watch + bounded media channel).
    /// Stored separately from [`Self::sideband_reader`] so open_sideband cannot
    /// overwrite / detach it — a leaked media_forward would keep consuming the
    /// peer's event channel after close and race teardown.
    media_forward: Option<JoinHandle<()>>,
    /// Shared flag set by `close_inner` so the sideband reader knows the close
    /// was deliberate and suppresses the unexpected-close error event.
    sideband_closed_flag: Option<Arc<AtomicBool>>,
    /// Shared sideband-open state. Set to `true` once the sideband WebSocket
    /// is open and owns server events; cleared on sideband close/teardown.
    /// The media-forward task reads this to gate data-channel events: while
    /// the sideband is open, parsed data-channel events are suppressed except
    /// `LiveServerEvent::Error` — matching OMP `transport.ts`
    /// `#handleServerEvent`, which ignores data-channel events whenever the
    /// sideband is OPEN except `error`. Before the sideband opens the
    /// data-channel is the fallback path and its events are accepted.
    sideband_open: Option<Arc<AtomicBool>>,
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
            media_forward: None,
            sideband_closed_flag: None,
            sideband_open: None,
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
        // Shared sideband-open state. The sideband reader sets this once the
        // WebSocket is open and clears it on close/teardown. The media-forward
        // task reads it to gate data-channel events: while the sideband is
        // open, data-channel events are suppressed except `Error` (OMP
        // `#handleServerEvent`). Starts false — the data-channel is the
        // fallback path until the sideband opens.
        let sideband_open = Arc::new(AtomicBool::new(false));
        self.sideband_open = Some(Arc::clone(&sideband_open));
        // Subscribe to the peer's lifecycle signal watch *before* any media
        // callback can fire (create_offer installs peer/data-channel callbacks
        // that may report failures). The media-forward task races this watch
        // against the bounded event channel so a media failure is surfaced as
        // a `LiveServerEvent::Error` (→ the session's fatal watch) even when
        // the event queue is saturated with control events — the watch is the
        // authoritative, non-sheddable failure path.
        let mut signal_rx = peer.subscribe_signals();
        let callbacks = Arc::clone(&self.callbacks);
        let media_forward = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    // Authoritative failure path: the peer published
                    // `PeerSignal::Failed` via its non-sheddable watch. This
                    // fires regardless of event-channel state, so a media
                    // failure is never silently lost when the bounded event
                    // queue is saturated with control events.
                    res = signal_rx.changed() => {
                        if res.is_err() {
                            // Sender dropped (peer torn down). Exit quietly.
                            break;
                        }
                        if let super::media::PeerSignal::Failed(msg) =
                            signal_rx.borrow().clone()
                        {
                            callbacks.on_event(LiveServerEvent::Error { message: msg });
                            break;
                        }
                    }
                    event = media_rx.recv_async() => {
                        let Ok(event) = event else { break; };
                        match event {
                            MediaEvent::Event(payload) => {
                                if let Some(ev) = parse_live_server_event(&payload) {
                                    // OMP `#handleServerEvent`: while the
                                    // sideband is OPEN it owns server events,
                                    // so data-channel events are suppressed
                                    // except `Error`. Before the sideband
                                    // opens the data-channel is the fallback.
                                    if route_data_channel_event(
                                        sideband_open.load(Ordering::Acquire),
                                        &ev,
                                    ) {
                                        callbacks.on_event(ev);
                                    }
                                }
                            }
                            MediaEvent::OutputLevel(level) => {
                                callbacks.on_output_level(level)
                            }
                            MediaEvent::Failure(msg) => {
                                // In-band failure. The peer's non-sheddable
                                // watch is the authoritative path; this is
                                // belt-and-suspenders. Either way exactly one
                                // Error reaches the session's fatal watch
                                // (the callback's `report_fatal` is once-only).
                                callbacks.on_event(LiveServerEvent::Error {
                                    message: msg,
                                });
                                break;
                            }
                        }
                    }
                }
            }
        });

        // Track media_forward immediately so any error path that runs
        // `close_inner` (via `connect`) can abort+join it. Leaving it local
        // until sideband success would detach the task on mid-connect failure.
        self.media_forward = Some(media_forward);

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

        let signaling_url = live_signaling_url(&self.config.codex_base);
        let body = serde_json::json!({
            "sdp": offer,
            "session": build_live_session_payload(&self.config.instructions, &self.config.voice),
        });

        let client =
            build_reqwest_client(&self.config.codex_base).map_err(|e| LiveSignalingError {
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
        // Sanitize the SDP answer: verify it looks like an SDP body (starts
        // with `v=0`) before returning it. A non-SDP 2xx body (e.g. an HTML
        // interstitial or a JSON error that slipped through) is rejected so
        // tokens/secrets in the body are never surfaced or passed to the
        // WebRTC stack. The SDP body itself contains no secrets (it's a
        // media description), but an unexpected body type might.
        let answer = body_text;
        if answer.trim().is_empty() {
            return Err(LiveSignalingError {
                status,
                message: "Codex live signaling returned an empty SDP answer".to_owned(),
            });
        }
        if !sdp_answer_is_valid(&answer) {
            return Err(LiveSignalingError {
                status,
                message: "Codex live signaling returned a non-SDP response body".to_owned(),
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
        let url_str =
            build_live_sideband_url_with_base(call_id, self.config.sideband_base.as_deref());
        let auth =
            self.auth.bearer_account().await.ok_or_else(|| {
                "No Codex credential is available for the live sideband.".to_string()
            })?;

        let mut request = url_str
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

        // Parse the sideband URL to get the target host, port, and scheme.
        let sideband_url =
            url::Url::parse(&url_str).map_err(|e| format!("invalid sideband URL: {e}"))?;
        let target_host = sideband_url
            .host_str()
            .ok_or_else(|| "sideband URL has no host".to_string())?;
        let target_port = sideband_url.port_or_known_default().unwrap_or(443);
        let is_wss = sideband_url.scheme() == "wss" || sideband_url.scheme() == "https";

        // Resolve the proxy for the sideband host (from sideband_base or
        // default api.openai.com). If a proxy is configured and not bypassed
        // by NO_PROXY, open a CONNECT tunnel; otherwise connect directly.
        let sideband_host = sideband_host(self.config.sideband_base.as_deref());
        let proxy_url = resolve_proxy_for_host(&sideband_host);

        // Finding 6: one timeout around the entire setup (DNS/TCP/proxy
        // CONNECT/TLS/WebSocket handshake). The individual steps are NOT
        // separately timed — the overall budget is SIDEBAND_CONNECT_TIMEOUT.
        let setup = async {
            if let Some(proxy_url) = proxy_url {
                // Parse the proxy URL safely (scheme/host/port/userinfo).
                let proxy = parse_proxy_url_full(&proxy_url)
                    .map_err(|e| format!("invalid proxy configuration: {e}"))?;

                // HTTPS proxies (TLS-to-proxy) are not supported via a raw
                // CONNECT tunnel — we'd need a TLS handshake to the proxy
                // itself before CONNECT. Reject explicitly with a sanitized
                // error (no credentials in the message).
                if proxy.is_https {
                    return Err("HTTPS proxy (TLS-to-proxy) is not supported for the live \
                         sideband WebSocket; use an HTTP proxy or configure \
                         HTTPS_PROXY to an http:// URL"
                        .to_string());
                }

                // Open the CONNECT tunnel (with Basic auth if the proxy URL
                // has userinfo).
                let tunnel = open_connect_tunnel(
                    proxy.host.as_str(),
                    proxy.port,
                    proxy.basic_auth(),
                    target_host,
                    target_port,
                )
                .await?;

                if is_wss {
                    // wss:// through the tunnel: TLS handshake to the target
                    // host, then WebSocket upgrade.
                    let tls_stream = tls_wrap(tunnel, target_host).await?;
                    let (ws, _resp) = client_async(request, MaybeTlsStream::Rustls(tls_stream))
                        .await
                        .map_err(|e| format!("Codex live sideband connection failed: {e}"))?;
                    Ok(ws)
                } else {
                    // ws:// through the tunnel: plain WebSocket upgrade (no
                    // TLS to the target). Use MaybeTlsStream::Plain.
                    let (ws, _resp) = client_async(request, MaybeTlsStream::Plain(tunnel))
                        .await
                        .map_err(|e| format!("Codex live sideband connection failed: {e}"))?;
                    Ok(ws)
                }
            } else {
                // Direct connection (no proxy). connect_async handles ws/wss
                // natively (TLS for wss, plain for ws).
                let connect = tokio_tungstenite::connect_async(request)
                    .await
                    .map_err(|e| format!("Codex live sideband connection failed: {e}"))?;
                Ok(connect.0)
            }
        };

        let ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>> =
            tokio::time::timeout(SIDEBAND_CONNECT_TIMEOUT, setup)
                .await
                .map_err(|_| "Codex live sideband connection timed out".to_string())??;

        let (ws_write, mut ws_read) = ws_stream.split();

        let (sideband_tx, sideband_rx) = mpsc::channel::<String>(64);
        self.sideband_tx = Some(sideband_tx);

        let callbacks = Arc::clone(&self.callbacks);
        // The transport's `closed` AtomicBool is not in an Arc, so we create a
        // shared flag for the reader. It's set to true by `close_inner` before
        // the reader is aborted, so if `closed` is true when the reader exits,
        // the close was deliberate and no error event is emitted.
        let closed_flag = Arc::new(AtomicBool::new(self.closed.load(Ordering::Acquire)));
        let closed_flag_for_close = Arc::clone(&closed_flag);
        let failure_reported = Arc::new(AtomicBool::new(false));
        // The sideband WebSocket is now open: it owns server events. Flip the
        // shared gate so the media-forward task suppresses data-channel events
        // except `Error` (OMP `#handleServerEvent`).
        let sideband_open = self
            .sideband_open
            .clone()
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        sideband_open.store(true, Ordering::Release);
        let sideband_open_for_reader = Arc::clone(&sideband_open);
        let reader = tokio::spawn(async move {
            loop {
                match ws_read.next().await {
                    Some(Ok(Message::Text(text))) => {
                        // Sideband events are always accepted (the sideband
                        // owns server events once open).
                        if let Some(event) = parse_live_server_event(&text) {
                            callbacks.on_event(event);
                        }
                        // Unknown/malformed payloads are silently dropped —
                        // never logged wholesale (secrets safety).
                    }
                    Some(Ok(Message::Binary(_))) => {
                        // OMP treats a binary sideband frame as a protocol
                        // failure. Do the same instead of silently hiding it.
                        sideband_open_for_reader.store(false, Ordering::Release);
                        if !closed_flag.load(Ordering::Acquire)
                            && !failure_reported.swap(true, Ordering::AcqRel)
                        {
                            callbacks.on_event(LiveServerEvent::Error {
                                message: "Codex live sideband returned an unexpected binary frame."
                                    .to_owned(),
                            });
                        }
                        break;
                    }
                    Some(Ok(Message::Close(frame))) => {
                        sideband_open_for_reader.store(false, Ordering::Release);
                        if !closed_flag.load(Ordering::Acquire)
                            && !failure_reported.swap(true, Ordering::AcqRel)
                        {
                            callbacks.on_event(LiveServerEvent::Error {
                                message: sideband_close_message(frame.as_ref()),
                            });
                        }
                        break;
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
                    Some(Err(error)) => {
                        sideband_open_for_reader.store(false, Ordering::Release);
                        if !closed_flag.load(Ordering::Acquire)
                            && !failure_reported.swap(true, Ordering::AcqRel)
                        {
                            callbacks.on_event(LiveServerEvent::Error {
                                message: format!("Codex live sideband failed: {error}"),
                            });
                        }
                        break;
                    }
                    None => {
                        // EOF without a Close frame. Clear the open gate so
                        // data-channel fallback resumes while the fatal event
                        // is delivered and the session tears down.
                        sideband_open_for_reader.store(false, Ordering::Release);
                        if !closed_flag.load(Ordering::Acquire)
                            && !failure_reported.swap(true, Ordering::AcqRel)
                        {
                            callbacks.on_event(LiveServerEvent::Error {
                                message: sideband_close_message(None),
                            });
                        }
                        break;
                    }
                }
            }
        });

        let writer = tokio::spawn(run_sideband_writer(
            ws_write,
            sideband_rx,
            SIDEBAND_KEEPALIVE_INTERVAL,
        ));

        self.sideband_reader = Some(reader);
        self.sideband_writer = Some(writer);
        self.sideband_closed_flag = Some(closed_flag_for_close);
        Ok(())
    }

    /// Serialize one Frameless Bidi control message onto the sideband. Returns
    /// an error if the transport is not connected or the sideband is gone.
    ///
    /// **Non-blocking for the session loop:** uses `try_send` so a stalled
    /// writer / full control queue cannot park `session` forever (which would
    /// starve Shutdown/CompleteDelegation). A full queue is treated as a
    /// protocol failure — control messages are never silently dropped.
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
        match tx.try_send(json) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(_)) => {
                Err("Codex live sideband is closed".to_owned())
            }
            Err(mpsc::error::TrySendError::Full(_)) => Err(
                "Codex live sideband control queue is full; refusing to drop a control message"
                    .to_owned(),
            ),
        }
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
        // Signal the sideband reader that this is a deliberate close so it
        // suppresses the unexpected-close error event.
        if let Some(flag) = self.sideband_closed_flag.take() {
            flag.store(true, Ordering::Release);
        }
        // Clear the sideband-open gate on teardown so a late data-channel
        // event resumes the fallback path (the session is closing regardless,
        // but the gate must not linger `true` past the sideband's lifetime).
        if let Some(open) = self.sideband_open.take() {
            open.store(false, Ordering::Release);
        }
        // Close the sideband first so no new messages are queued. Dropping the
        // sender unblocks a writer parked on a full control channel without
        // requiring an abort first (so in-flight control text can flush).
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
        // Abort + join media_forward before peer close so it cannot race
        // peer teardown or keep holding the media event receiver.
        if let Some(media_forward) = self.media_forward.take() {
            media_forward.abort();
            let _ = media_forward.await;
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
        if let Some(flag) = self.sideband_closed_flag.take() {
            flag.store(true, Ordering::Release);
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current()
            && let Some(peer) = self.peer.take()
        {
            handle.spawn(async move {
                peer.close().await;
            });
        }
        if let Some(media_forward) = self.media_forward.take() {
            media_forward.abort();
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

/// Build a reqwest client that honors standard HTTP proxy / NO_PROXY env vars
/// for the **signaling** endpoint. The proxy host is derived from `codex_base`
/// so NO_PROXY matches the actual signaling target. reqwest reads
/// `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` when a `Proxy` is configured.
fn build_reqwest_client(codex_base: &str) -> Result<reqwest::Client, reqwest::Error> {
    let mut builder =
        reqwest::Client::builder().user_agent(format!("Codex Desktop/{}", "xai-grok-voice"));
    let signaling_host = url_host(codex_base);
    if let Some(proxy_url) = resolve_proxy_for_host(&signaling_host)
        && let Ok(proxy) = reqwest::Proxy::all(&proxy_url)
    {
        builder = builder.proxy(proxy);
    }
    builder.build()
}

/// Resolve the proxy URL for a given target host from the standard env vars
/// (HTTPS_PROXY > HTTP_PROXY), respecting NO_PROXY. This is the same logic as
/// `xai-grok-shell::agent::proxy::resolve_proxy_for_host`, duplicated inline to
/// avoid a dependency cycle (shell → voice → shell).
fn resolve_proxy_for_host(target_host: &str) -> Option<String> {
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

/// Extract the host (lowercased, without scheme/path/port) from a URL string.
/// Used to derive the NO_PROXY target for the signaling endpoint.
fn url_host(url: &str) -> String {
    let after_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("wss://"))
        .or_else(|| url.strip_prefix("ws://"))
        .unwrap_or(url);
    after_scheme
        .split('/')
        .next()
        .unwrap_or(url)
        // Strip port if present.
        .split(':')
        .next()
        .unwrap_or(after_scheme)
        .to_ascii_lowercase()
}

/// Derive the target host for NO_PROXY matching from the sideband base URL
/// (or the default `api.openai.com`). Strips the scheme and path, keeping only
/// the host (without port, for NO_PROXY suffix matching).
fn sideband_host(sideband_base: Option<&str>) -> String {
    let base = sideband_base
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim())
        .unwrap_or("https://api.openai.com/v1/live/");
    url_host(base)
}

// ---------------------------------------------------------------------------
// HTTP CONNECT proxy tunnel for the sideband WebSocket
// ---------------------------------------------------------------------------

/// Lazily-initialized TLS client configuration using webpki root certificates
/// (matching tokio-tungstenite's `rustls-tls-webpki-roots` backend).
static TLS_CONFIG: std::sync::OnceLock<Arc<rustls::ClientConfig>> = std::sync::OnceLock::new();

fn get_tls_config() -> Result<Arc<rustls::ClientConfig>, String> {
    // OnceLock::get_or_init always succeeds.
    Ok(TLS_CONFIG
        .get_or_init(|| {
            let mut root_store = rustls::RootCertStore::empty();
            root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone())
}

/// Perform a TLS handshake over an existing TCP stream using rustls with
/// webpki root certificates.
async fn tls_wrap(
    stream: TcpStream,
    server_name: &str,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>, String> {
    let tls_config = get_tls_config().map_err(|e| format!("TLS config: {e}"))?;
    let connector = tokio_rustls::TlsConnector::from(tls_config);
    let dns_name = rustls::pki_types::ServerName::try_from(server_name.to_string())
        .map_err(|e| format!("invalid TLS server name '{server_name}': {e}"))?;
    connector
        .connect(dns_name, stream)
        .await
        .map_err(|e| format!("TLS handshake failed: {e}"))
}

/// Open a raw TCP tunnel through an HTTP CONNECT proxy. Sends
/// `CONNECT host:port HTTP/1.1` (with optional `Proxy-Authorization` header)
/// and verifies a 200 response. Returns the `TcpStream` positioned after the
/// CONNECT response headers. The proxy URL is never passed here — only the
/// parsed host/port/auth — so credentials cannot leak into error messages
/// (finding 6).
async fn open_connect_tunnel(
    proxy_host: &str,
    proxy_port: u16,
    basic_auth: Option<String>,
    target_host: &str,
    target_port: u16,
) -> Result<TcpStream, String> {
    let proxy_addr = format!("{proxy_host}:{proxy_port}");
    let stream = TcpStream::connect(&proxy_addr)
        .await
        .map_err(|e| format!("failed to connect to proxy: {e}"))?;

    let connect_req = match &basic_auth {
        Some(auth) => format!(
            "CONNECT {target_host}:{target_port} HTTP/1.1\r\n\
             Host: {target_host}:{target_port}\r\n\
             Proxy-Authorization: {auth}\r\n\
             \r\n"
        ),
        None => format!(
            "CONNECT {target_host}:{target_port} HTTP/1.1\r\n\
             Host: {target_host}:{target_port}\r\n\
             \r\n"
        ),
    };
    let (reader_half, mut writer_half) = stream.into_split();
    writer_half
        .write_all(connect_req.as_bytes())
        .await
        .map_err(|e| format!("proxy CONNECT write failed: {e}"))?;
    writer_half
        .flush()
        .await
        .map_err(|e| format!("proxy CONNECT flush failed: {e}"))?;

    let mut reader = BufReader::new(reader_half);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .await
        .map_err(|e| format!("proxy CONNECT read failed: {e}"))?;
    if !status_line.starts_with("HTTP/1.1 200") && !status_line.starts_with("HTTP/1.0 200") {
        // Sanitize: the status line from the proxy should not contain our
        // credentials (they're in the request, not the response), but strip
        // any trailing whitespace.
        return Err(format!("proxy CONNECT rejected: {}", status_line.trim()));
    }
    // Consume remaining response headers.
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("proxy CONNECT header read failed: {e}"))?;
        if line.trim().is_empty() {
            break;
        }
    }
    // Assert the BufReader's internal buffer is empty before reuniting.
    let remaining = reader.buffer();
    if !remaining.is_empty() {
        return Err(format!(
            "proxy sent {} unexpected byte(s) after CONNECT response headers",
            remaining.len()
        ));
    }
    reader
        .into_inner()
        .reunite(writer_half)
        .map_err(|e| format!("proxy stream reunite failed: {e}"))
}

/// Parsed proxy URL: scheme, host, port, and optional userinfo (for Basic auth).
/// The raw URL is never stored or surfaced in errors (finding 6: no credentials
/// in error messages). `Debug` is derived for test assertions only; this struct
/// is private to the module.
#[derive(Debug)]
struct ProxyConfig {
    is_https: bool,
    host: String,
    port: u16,
    username: Option<String>,
    password: Option<String>,
}

impl ProxyConfig {
    /// Returns a `Authorization: Basic <base64>` header value if the proxy URL
    /// had userinfo, or `None` otherwise.
    fn basic_auth(&self) -> Option<String> {
        match (&self.username, &self.password) {
            (Some(u), Some(p)) => {
                let credentials = format!("{u}:{p}");
                let encoded = base64_encode(credentials.as_bytes());
                Some(format!("Basic {encoded}"))
            }
            (Some(u), None) => {
                let credentials = format!("{u}:");
                let encoded = base64_encode(credentials.as_bytes());
                Some(format!("Basic {encoded}"))
            }
            _ => None,
        }
    }
}

/// Minimal Base64 encoder (standard alphabet, with padding) for proxy Basic
/// auth. We avoid pulling in a base64 crate dependency for this small use.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let i0 = (b0 >> 2) as usize;
        let i1 = (((b0 & 0x03) << 4) | (b1 >> 4)) as usize;
        out.push(ALPHABET[i0] as char);
        out.push(ALPHABET[i1] as char);
        if chunk.len() > 1 {
            let i2 = (((b1 & 0x0f) << 2) | (b2 >> 6)) as usize;
            out.push(ALPHABET[i2] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            let i3 = (b2 & 0x3f) as usize;
            out.push(ALPHABET[i3] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Parse a proxy URL into a `ProxyConfig` (scheme/host/port/userinfo). Accepted
/// schemes: `http://`, `https://`, or bare `host:port`. Never includes the raw
/// URL in errors — uses a generic message so credentials are never leaked
/// (finding 6).
fn parse_proxy_url_full(url: &str) -> Result<ProxyConfig, String> {
    let (is_https, without_scheme) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        (false, url)
    };

    // Strip path/query.
    let authority = without_scheme.split('/').next().unwrap_or(without_scheme);

    // Extract userinfo (user:pass@) if present.
    let (userinfo, hostport) = if let Some(at_pos) = authority.rfind('@') {
        (Some(&authority[..at_pos]), &authority[at_pos + 1..])
    } else {
        (None, authority)
    };

    // Parse host:port.
    let (host, port) = if let Some((h, p)) = hostport.rsplit_once(':') {
        let port: u16 = p.parse().map_err(|_| "invalid proxy port".to_string())?;
        (h.to_string(), port)
    } else {
        (hostport.to_string(), 80)
    };

    if host.is_empty() {
        return Err("proxy URL has no host".to_string());
    }

    let (username, password) = match userinfo {
        Some(ui) => {
            if let Some((u, p)) = ui.split_once(':') {
                (Some(u.to_string()), Some(p.to_string()))
            } else {
                (Some(ui.to_string()), None)
            }
        }
        None => (None, None),
    };

    Ok(ProxyConfig {
        is_https,
        host,
        port,
        username,
        password,
    })
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

/// Validate that a response body looks like an SDP answer before returning it
/// to the WebRTC stack. SDP bodies always start with `v=0` (per RFC 4566). A
/// body that doesn't match is rejected so non-SDP responses (which might carry
/// tokens or session secrets in error JSON/HTML) are never surfaced or passed
/// to the peer connection's SDP parser.
fn sdp_answer_is_valid(body: &str) -> bool {
    let trimmed = body.trim_start();
    trimmed.starts_with("v=0") || trimmed.starts_with("v= 0")
}

/// Normalize an error response body into a single bounded, **redacted** line.
/// Strips bearer tokens, session secrets, cookies, and URLs with userinfo
/// before surfacing. Never logs the full body wholesale beyond the cap.
///
/// Redaction is conservative: any substring matching common secret patterns
/// (`Bearer ...`, `token=...`, `cookie: ...`, `session_id=...`, etc.) is
/// replaced with `[REDACTED]`. URLs containing userinfo (`user:pass@host`) are
/// also redacted. After redaction the body is whitespace-collapsed and
/// truncated to `MAX_ERROR_BODY_LENGTH` chars.
fn bounded_error_body(body: &str, status: u16) -> String {
    let redacted = redact_secrets(body);
    let normalized: String = redacted.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return format!("HTTP {status}");
    }
    if normalized.chars().count() <= MAX_ERROR_BODY_LENGTH {
        return normalized;
    }
    let truncated: String = normalized.chars().take(MAX_ERROR_BODY_LENGTH).collect();
    format!("{truncated}…")
}

/// Redact, normalize, and bound a terminal Live error before it is written to
/// a persistent diagnostic log. UI toasts may show the original local error,
/// but disk logs must never retain bearer tokens, cookies, session ids, proxy
/// credentials, or an unbounded server-provided message.
pub fn redact_live_error_for_log(message: &str) -> String {
    let redacted = redact_secrets(message);
    let normalized = redacted.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= MAX_ERROR_BODY_LENGTH {
        return normalized;
    }
    let mut bounded: String = normalized.chars().take(MAX_ERROR_BODY_LENGTH).collect();
    bounded.push('…');
    bounded
}

/// Redact secret-bearing substrings from a response body. Replaces:
/// - `Bearer <token>` → `Bearer [REDACTED]`
/// - `token=<value>` / `access_token=<value>` → `token=[REDACTED]`
/// - `cookie:<value>` / `Cookie: <value>` → `Cookie: [REDACTED]`
/// - `session_id=<value>` / `session-id: <value>` → `session_id=[REDACTED]`
/// - `password=<value>` / `passwd=<value>` → `password=[REDACTED]`
/// - URLs with userinfo: `https://user:pass@host` → `https://[REDACTED]@host`
///
/// This is intentionally conservative — it's better to over-redact than to
/// leak a credential. The patterns are case-insensitive.
fn redact_secrets(body: &str) -> String {
    // Work on bytes/chars to avoid regex catastrophic backtracking and to keep
    // the scan single-pass and bounded. We build the output incrementally so we
    // never re-scan already-redacted text (which would risk infinite loops).
    let lower = body.to_ascii_lowercase();
    let bytes = body.as_bytes();
    let n = bytes.len();
    let mut out = String::with_capacity(n);
    let mut i = 0usize;

    let secret_keys: &[&str] = &[
        "access_token",
        "refresh_token",
        "token",
        "session_id",
        "session-id",
        "password",
        "passwd",
        "secret",
        "api_key",
        "apikey",
        "credential",
        "authorization",
    ];

    let schemes: &[&str] = &["https://", "http://", "wss://", "ws://"];

    while i < n {
        // --- Bearer token: "Bearer " + non-ws token ---
        if lower[i..].starts_with("bearer ") {
            out.push_str(&body[i..i + 7]);
            let mut j = i + 7;
            while j < n && !bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j > i + 7 {
                out.push_str("[REDACTED]");
            }
            i = j;
            continue;
        }

        // --- Cookie header: "cookie:" up to newline ---
        if lower[i..].starts_with("cookie:") {
            out.push_str(&body[i..i + 7]);
            let mut j = i + 7;
            while j < n && bytes[j] != b'\n' {
                j += 1;
            }
            out.push_str(" [REDACTED]");
            i = j;
            continue;
        }

        // --- key=value / key: value / "key":"value" / "key": "value" for known secret keys ---
        // Detect a word boundary before the key (allow a leading quote).
        let at_word_boundary =
            i == 0 || (!bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_');
        if at_word_boundary {
            // Optionally consume a leading double-quote for JSON-style keys.
            let key_pos = if bytes[i] == b'"' { i + 1 } else { i };
            if let Some((key_len, sep_len, _had_quote)) =
                match_secret_key(&lower, key_pos, secret_keys)
            {
                let value_start = key_pos + key_len + sep_len;
                // Emit the quote + key + separator from original text.
                out.push_str(&body[i..value_start]);
                // If the value begins with a double-quote (JSON string),
                // skip it and scan until the closing quote; otherwise scan
                // until whitespace / , / } / ".
                let mut j = value_start;
                let value_quoted = j < n && bytes[j] == b'"';
                if value_quoted {
                    j += 1; // skip opening quote
                    let val_begin = j;
                    while j < n && bytes[j] != b'"' {
                        j += 1;
                    }
                    if j > val_begin {
                        out.push('"');
                        out.push_str("[REDACTED]");
                        if j < n {
                            out.push('"');
                            j += 1; // skip closing quote
                        }
                    } else {
                        // empty string value
                        out.push('"');
                        if j < n {
                            out.push('"');
                            j += 1;
                        }
                    }
                } else {
                    let val_begin = j;
                    while j < n
                        && !bytes[j].is_ascii_whitespace()
                        && bytes[j] != b','
                        && bytes[j] != b'}'
                        && bytes[j] != b'"'
                    {
                        j += 1;
                    }
                    if j > val_begin {
                        out.push_str("[REDACTED]");
                    }
                }
                i = j;
                continue;
            }
        }

        // --- URL with userinfo: scheme://user:pass@host → scheme://[REDACTED]@host ---
        if let Some(scheme) = schemes.iter().find(|s| lower[i..].starts_with(*s)) {
            let after_scheme = i + scheme.len();
            // Find next '/' or end within the authority.
            let mut slash = after_scheme;
            while slash < n && bytes[slash] != b'/' {
                slash += 1;
            }
            // Look for '@' in the authority.
            let mut at = None;
            let mut k = after_scheme;
            while k < slash {
                if bytes[k] == b'@' {
                    at = Some(k);
                    break;
                }
                k += 1;
            }
            if let Some(at_pos) = at {
                out.push_str(scheme);
                out.push_str("[REDACTED]");
                i = at_pos; // keep the '@' and host in output
                continue;
            }
            // No userinfo — emit the scheme and continue scanning the rest.
            out.push_str(scheme);
            i = after_scheme;
            continue;
        }

        // Default: copy one UTF-8 character. We only branch above on ASCII
        // prefixes, so non-ASCII bytes fall through here. Copy the whole char
        // to avoid splitting multi-byte sequences.
        let rest = &body[i..];
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }

    out
}

/// If `s[pos..]` (already lowercased) starts with one of `keys` followed by a
/// recognized separator, returns `(key_len, sep_len, had_quote)`. Recognized
/// separators: `=`, `: ` (colon-space), `":` (JSON `"key":value`),
/// `": ` (JSON `"key": value`). `had_quote` indicates the JSON quoted form.
fn match_secret_key(s: &str, pos: usize, keys: &[&str]) -> Option<(usize, usize, bool)> {
    let rest = &s[pos..];
    for key in keys {
        if let Some(after) = rest.strip_prefix(key) {
            if after.starts_with('=') {
                return Some((key.len(), 1, false));
            }
            if after.starts_with(": ") {
                return Some((key.len(), 2, false));
            }
            // JSON quoted form: key immediately followed by `"` then `:` or `": `.
            if after.starts_with("\":") {
                if after.starts_with("\": ") {
                    return Some((key.len(), 3, true));
                }
                return Some((key.len(), 2, true));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::super::protocol::{LiveInputTextContent, LiveRole, TranscriptKind};
    use super::*;

    struct RecordingMessageSink {
        tx: mpsc::UnboundedSender<Message>,
    }

    impl futures_util::Sink<Message> for RecordingMessageSink {
        type Error = ();

        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn start_send(self: std::pin::Pin<&mut Self>, message: Message) -> Result<(), Self::Error> {
            self.get_mut().tx.send(message).map_err(|_| ())
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn sideband_writer_sends_keepalive_ping_while_application_is_idle() {
        let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
        let (application_tx, application_rx) = mpsc::channel(1);
        let writer = tokio::spawn(run_sideband_writer(
            RecordingMessageSink { tx: observed_tx },
            application_rx,
            Duration::from_millis(10),
        ));

        let message = tokio::time::timeout(Duration::from_secs(1), observed_rx.recv())
            .await
            .expect("idle writer should send a keepalive")
            .expect("recording sink should stay open");
        assert!(
            matches!(&message, Message::Ping(payload) if payload.is_empty()),
            "idle sideband must emit an empty protocol Ping, got {message:?}"
        );

        drop(application_tx);
        tokio::time::timeout(Duration::from_secs(1), writer)
            .await
            .expect("writer should stop after its application channel closes")
            .expect("writer task should not panic");
    }

    #[tokio::test]
    async fn sideband_writer_still_forwards_application_text() {
        let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
        let (application_tx, application_rx) = mpsc::channel(1);
        let writer = tokio::spawn(run_sideband_writer(
            RecordingMessageSink { tx: observed_tx },
            application_rx,
            Duration::from_secs(60),
        ));

        application_tx.send("control".to_owned()).await.unwrap();
        let message = tokio::time::timeout(Duration::from_secs(1), observed_rx.recv())
            .await
            .expect("application message should be forwarded")
            .expect("recording sink should stay open");
        assert!(matches!(message, Message::Text(text) if text == "control"));

        drop(application_tx);
        tokio::time::timeout(Duration::from_secs(1), writer)
            .await
            .expect("writer should stop after its application channel closes")
            .expect("writer task should not panic");
    }

    #[test]
    fn sideband_close_message_preserves_code_and_reason() {
        let frame = CloseFrame {
            code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Policy,
            reason: "  account policy\nchanged  ".into(),
        };
        assert_eq!(
            sideband_close_message(Some(&frame)),
            "Codex live sideband closed (1008): account policy changed"
        );
    }

    #[test]
    fn sideband_close_message_handles_missing_frame() {
        assert_eq!(
            sideband_close_message(None),
            "Codex live sideband closed without a close frame"
        );
    }

    // --- OMP `#handleServerEvent` data-channel routing gate -----------------
    //
    // While the sideband WebSocket is OPEN it owns server events, so the
    // data-channel (`oai-events`) must suppress every parsed event except
    // `Error`. Before the sideband opens (and after it closes) the
    // data-channel is the fallback path and accepts all events. These tests
    // exercise the pure routing predicate plus the shared-state lifecycle.

    fn delegation(id: &str) -> LiveServerEvent {
        LiveServerEvent::DelegationCreated {
            id: id.to_owned(),
            content: vec![LiveInputTextContent::input_text("do work")],
        }
    }

    fn turn(role: LiveRole, transcript: &str) -> LiveServerEvent {
        LiveServerEvent::TurnDone {
            role,
            transcript: transcript.to_owned(),
        }
    }

    fn transcript(kind: TranscriptKind, text: &str) -> LiveServerEvent {
        LiveServerEvent::TranscriptAdded {
            kind,
            text: text.to_owned(),
        }
    }

    #[test]
    fn route_pre_sideband_accepts_all_data_channel_events() {
        // Before the sideband opens, the data-channel is the fallback path.
        assert!(route_data_channel_event(false, &delegation("d1")));
        assert!(route_data_channel_event(
            false,
            &turn(LiveRole::Assistant, "hi")
        ));
        assert!(route_data_channel_event(
            false,
            &transcript(TranscriptKind::Output, "delta")
        ));
        assert!(route_data_channel_event(
            false,
            &LiveServerEvent::Error {
                message: "boom".into()
            }
        ));
        assert!(route_data_channel_event(
            false,
            &LiveServerEvent::Unknown {
                wire_type: "future.event".into()
            }
        ));
    }

    #[test]
    fn route_sideband_open_suppresses_all_except_error() {
        // While the sideband is open it owns server events: delegation, turn,
        // transcript, session, output_audio.delta, and unknown are suppressed.
        assert!(
            !route_data_channel_event(true, &delegation("d1")),
            "delegation must be suppressed while sideband is open"
        );
        assert!(
            !route_data_channel_event(true, &turn(LiveRole::Assistant, "hi")),
            "turn must be suppressed while sideband is open"
        );
        assert!(
            !route_data_channel_event(true, &transcript(TranscriptKind::Output, "delta")),
            "transcript must be suppressed while sideband is open"
        );
        assert!(!route_data_channel_event(
            true,
            &LiveServerEvent::Unknown {
                wire_type: "future.event".into()
            }
        ));
        // `Error` is always accepted from the data channel (OMP: `except error`).
        assert!(
            route_data_channel_event(
                true,
                &LiveServerEvent::Error {
                    message: "boom".into()
                }
            ),
            "data-channel error must be accepted even while sideband is open"
        );
    }

    #[test]
    fn route_post_close_resumes_fallback_acceptance() {
        // After the sideband closes, the gate flips back to false and the
        // data-channel fallback resumes accepting all events.
        let open = Arc::new(AtomicBool::new(true));
        assert!(!route_data_channel_event(
            open.load(Ordering::Acquire),
            &delegation("d1")
        ));
        open.store(false, Ordering::Release);
        assert!(route_data_channel_event(
            open.load(Ordering::Acquire),
            &delegation("d1")
        ));
        assert!(route_data_channel_event(
            open.load(Ordering::Acquire),
            &turn(LiveRole::User, "after close")
        ));
    }

    /// The shared sideband-open flag models the lifecycle the reader task
    /// drives: starts false (fallback), flips true once the WebSocket opens,
    /// clears on close/teardown. Verify the gate transitions match OMP
    /// semantics across the full pre-open → open → close cycle.
    #[test]
    fn sideband_open_gate_lifecycle_matches_omp() {
        let open = Arc::new(AtomicBool::new(false));
        // Pre-sideband: data-channel delegation accepted.
        assert!(route_data_channel_event(
            open.load(Ordering::Acquire),
            &delegation("pre")
        ));
        // Sideband opens: delegation now suppressed, error still accepted.
        open.store(true, Ordering::Release);
        assert!(!route_data_channel_event(
            open.load(Ordering::Acquire),
            &delegation("dup")
        ));
        assert!(route_data_channel_event(
            open.load(Ordering::Acquire),
            &LiveServerEvent::Error {
                message: "dc err".into()
            }
        ));
        // Sideband closes (unexpected): gate clears, fallback resumes. The
        // session will terminally fail on the unexpected close; a deliberate
        // close ignores state, but in both cases the gate is false.
        open.store(false, Ordering::Release);
        assert!(route_data_channel_event(
            open.load(Ordering::Acquire),
            &delegation("post")
        ));
    }

    /// A duplicate/stale sideband-open must not re-enable suppression after a
    /// close, and re-opening is idempotent: the gate is a single shared bool,
    /// so a stale socket clearing it does not corrupt a fresh session's state
    /// (the transport creates a fresh gate per `connect_inner`).
    #[test]
    fn sideband_open_gate_is_idempotent_under_duplicate_open() {
        let open = Arc::new(AtomicBool::new(false));
        open.store(true, Ordering::Release);
        // A second "open" is a no-op (already true) — no duplicate delivery.
        open.store(true, Ordering::Release);
        assert!(open.load(Ordering::Acquire));
        assert!(!route_data_channel_event(
            open.load(Ordering::Acquire),
            &delegation("d")
        ));
        // A stale socket's EOF clears the gate.
        open.store(false, Ordering::Release);
        assert!(!open.load(Ordering::Acquire));
    }

    // --- existing tests below ---------------------------------------------

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
    fn signaling_url_appends_exact_suffix_to_codex_base() {
        assert_eq!(
            live_signaling_url("https://chatgpt.com/backend-api/codex/"),
            "https://chatgpt.com/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas"
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

    #[test]
    fn sdp_answer_is_valid_accepts_standard_sdp() {
        assert!(sdp_answer_is_valid("v=0\r\no=- 123 1 IN IP4 0.0.0.0\r\n"));
        assert!(sdp_answer_is_valid("  v=0\n..."));
        assert!(sdp_answer_is_valid("v= 0\r\n"));
    }

    #[test]
    fn sdp_answer_is_valid_rejects_non_sdp() {
        assert!(!sdp_answer_is_valid(r#"{"error":"token_secret"}"#));
        assert!(!sdp_answer_is_valid("<html>interstitial</html>"));
        assert!(!sdp_answer_is_valid("not an sdp"));
        assert!(!sdp_answer_is_valid(""));
    }

    #[test]
    fn sideband_host_derives_from_default() {
        assert_eq!(sideband_host(None), "api.openai.com");
    }

    #[test]
    fn sideband_host_derives_from_custom_base() {
        assert_eq!(
            sideband_host(Some("https://custom.example.com/v1/live/")),
            "custom.example.com"
        );
        assert_eq!(
            sideband_host(Some("wss://proxy.corp.net:8443/live")),
            "proxy.corp.net"
        );
    }

    #[test]
    fn sideband_host_strips_path_and_scheme() {
        assert_eq!(
            sideband_host(Some("https://api.staging.openai.com/v1/live")),
            "api.staging.openai.com"
        );
    }

    #[test]
    fn url_host_extracts_from_codex_base() {
        assert_eq!(
            url_host("https://chatgpt.com/backend-api/codex"),
            "chatgpt.com"
        );
        assert_eq!(
            url_host("https://api.staging.openai.com/v1/live"),
            "api.staging.openai.com"
        );
    }

    #[test]
    fn parse_proxy_url_extracts_host_and_port() {
        let p = parse_proxy_url_full("http://proxy.example.com:3128").unwrap();
        assert_eq!(p.host, "proxy.example.com");
        assert_eq!(p.port, 3128);
        assert!(!p.is_https);
        assert!(p.username.is_none());
        assert!(p.password.is_none());
    }

    #[test]
    fn parse_proxy_url_defaults_port_to_80() {
        let p = parse_proxy_url_full("http://proxy.example.com").unwrap();
        assert_eq!(p.host, "proxy.example.com");
        assert_eq!(p.port, 80);
    }

    /// Finding 6: HTTPS proxy scheme is detected so it can be rejected.
    #[test]
    fn parse_proxy_url_detects_https_scheme() {
        let p = parse_proxy_url_full("https://proxy.example.com:3128").unwrap();
        assert!(p.is_https);
        assert_eq!(p.port, 3128);
    }

    /// Finding 6: proxy URL userinfo (Basic auth) is parsed correctly.
    #[test]
    fn parse_proxy_url_extracts_userinfo() {
        let p = parse_proxy_url_full("http://user:pass@proxy.example.com:3128").unwrap();
        assert_eq!(p.host, "proxy.example.com");
        assert_eq!(p.port, 3128);
        assert_eq!(p.username.as_deref(), Some("user"));
        assert_eq!(p.password.as_deref(), Some("pass"));
        let auth = p.basic_auth().unwrap();
        assert!(auth.starts_with("Basic "));
        // Base64 of "user:pass" = "dXNlcjpwYXNz"
        assert!(auth.contains("dXNlcjpwYXNz"));
    }

    /// Finding 6: proxy URL with username only (no password).
    #[test]
    fn parse_proxy_url_extracts_username_only() {
        let p = parse_proxy_url_full("http://user@proxy.example.com:3128").unwrap();
        assert_eq!(p.username.as_deref(), Some("user"));
        assert!(p.password.is_none());
        assert!(p.basic_auth().is_some());
    }

    /// Finding 6: bare host:port (no scheme) parses as HTTP.
    #[test]
    fn parse_proxy_url_bare_host_port() {
        let p = parse_proxy_url_full("proxy.example.com:3128").unwrap();
        assert_eq!(p.host, "proxy.example.com");
        assert_eq!(p.port, 3128);
        assert!(!p.is_https);
    }

    /// Finding 6: errors never include the raw URL (no credential leak).
    #[test]
    fn parse_proxy_url_error_has_no_credentials() {
        let url = "http://secretuser:secretpass@proxy.example.com:notaport";
        let err = parse_proxy_url_full(url).unwrap_err();
        assert!(!err.contains("secretuser"));
        assert!(!err.contains("secretpass"));
        assert!(!err.contains(url));
    }

    /// Finding 6: base64 encoder correctness.
    #[test]
    fn base64_encode_userpass() {
        assert_eq!(base64_encode(b"user:pass"), "dXNlcjpwYXNz");
        assert_eq!(base64_encode(b"a:b"), "YTpi");
        assert_eq!(base64_encode(b"test"), "dGVzdA==");
    }

    /// Finding 6: ws:// (unencrypted) scheme detection.
    #[test]
    fn sideband_url_scheme_ws_vs_wss() {
        let ws = url::Url::parse("ws://example.com/live").unwrap();
        assert_eq!(ws.scheme(), "ws");
        let wss = url::Url::parse("wss://example.com/live").unwrap();
        assert_eq!(wss.scheme(), "wss");
    }

    // --- Error body redaction tests (finding 8) ---

    #[test]
    fn redact_bearer_token_from_error_body() {
        let body = r#"{"error":"Authorization failed","header":"Bearer sk-abc123secret"}"#;
        let redacted = redact_secrets(body);
        assert!(
            !redacted.contains("sk-abc123secret"),
            "bearer token must be redacted: {redacted}"
        );
        assert!(redacted.contains("[REDACTED]"));
    }

    #[test]
    fn redact_token_key_value_from_error_body() {
        let body = r#"{"error":"bad token","token":"sk-secret-value-123"}"#;
        let redacted = redact_secrets(body);
        assert!(
            !redacted.contains("sk-secret-value-123"),
            "token value must be redacted: {redacted}"
        );
    }

    #[test]
    fn redact_session_id_from_error_body() {
        let body = "session_id=abc123secret&other=data";
        let redacted = redact_secrets(body);
        assert!(
            !redacted.contains("abc123secret"),
            "session_id must be redacted: {redacted}"
        );
    }

    #[test]
    fn redact_password_from_error_body() {
        let body = "password=hunter2&user=admin";
        let redacted = redact_secrets(body);
        assert!(
            !redacted.contains("hunter2"),
            "password must be redacted: {redacted}"
        );
    }

    #[test]
    fn redact_url_with_userinfo() {
        let body = "Failed to connect to https://admin:secret@internal.example.com/api";
        let redacted = redact_secrets(body);
        assert!(
            !redacted.contains("admin:secret"),
            "URL userinfo must be redacted: {redacted}"
        );
        assert!(redacted.contains("[REDACTED]@internal.example.com"));
    }

    #[test]
    fn redact_cookie_header() {
        let body = "Cookie: session=secret123; token=abc\nNext line";
        let redacted = redact_secrets(body);
        assert!(
            !redacted.contains("secret123"),
            "cookie value must be redacted: {redacted}"
        );
    }

    #[test]
    fn redact_multiple_secrets_in_one_body() {
        let body = r#"{"token":"tok123","password":"pw456","session_id":"sid789"}"#;
        let redacted = redact_secrets(body);
        assert!(!redacted.contains("tok123"));
        assert!(!redacted.contains("pw456"));
        assert!(!redacted.contains("sid789"));
    }

    #[test]
    fn redact_preserves_non_secret_content() {
        let body = "Internal server error: database connection refused";
        let redacted = redact_secrets(body);
        assert_eq!(redacted, body);
    }

    #[test]
    fn persistent_live_error_is_redacted_normalized_and_bounded() {
        let message = format!(
            "Bearer sk-live-secret\n{}",
            "x".repeat(MAX_ERROR_BODY_LENGTH * 2)
        );
        let out = redact_live_error_for_log(&message);
        assert!(!out.contains("sk-live-secret"));
        assert!(!out.contains('\n'));
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), MAX_ERROR_BODY_LENGTH + 1);
    }

    #[test]
    fn bounded_error_body_redacts_secrets_before_truncation() {
        let body = format!(
            "Bearer {} {}",
            "sk-super-secret-token",
            "x".repeat(MAX_ERROR_BODY_LENGTH * 2)
        );
        let out = bounded_error_body(&body, 500);
        assert!(
            !out.contains("sk-super-secret-token"),
            "secret must be redacted even when body is truncated: {out}"
        );
        assert!(out.contains("[REDACTED]"));
    }
}
