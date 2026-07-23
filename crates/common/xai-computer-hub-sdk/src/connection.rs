//! Per-`(url, principal)` WebSocket connection actor.
//!
//! # Why this exists
//!
//! Multiple [`crate::ToolServer`] instances in the same process MAY
//! attach to the same server URL with the same credential. Opening one
//! socket per server would multiply server-side connection cost, fan-out
//! the per-tool ack chatter, and make per-frame envelope checks
//! ambiguous (the server can't tell which of N sockets owns a session
//! binding). The pool collapses every `(url, principal)` to one
//! [`HubConnection`]; refcounted session bindings make the collapse
//! safe.
//!
//! # The reconnect / replay state machine
//!
//! When the underlying socket drops, in-flight `tool_call_request`
//! responses CANNOT be recovered (the server holds no replay log). The
//! connection actor therefore:
//!
//! 1. Drains every parked response waiter with
//!    [`crate::ClientError::NetworkError`] so callers can fast-fail
//!    instead of deadlocking.
//! 2. Reconnects with exponential backoff (capped).
//! 3. Re-runs the `hello` handshake.
//! 4. The ToolServer replays `serve{session_id, tools}` per active
//!    session via the on_reconnect callback. The server auto-registers
//!    sessions from `serve` so no separate wire call is needed.
//! 5. Drains any outbound frames that buffered during step 1-4.
use crate::auth::{AuthCredential, AuthProvider, PrincipalKey};
use crate::demux::Demux;
use crate::error::ClientError;
use crate::handshake::send_hello;
use crate::refcount::RefCountedSet;
use futures::stream::SplitSink;
use futures::stream::SplitStream;
use futures::{SinkExt, Stream, StreamExt};
use http::HeaderName;
use http::header::HeaderValue;
use serde_json::Value;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use url::Url;
use xai_tool_protocol::{
    ConnectionId, ConnectionKind, JsonRpcId, JsonRpcRequest, JsonRpcResponse, JsonRpcVersion,
    Method, PongFrame, ResponseOutcome, SessionId,
};
/// Outbound mpsc bound. Picked to match the server's per-actor outbound
/// buffer so a single-process roundtrip never dead-blocks on sender
/// capacity.
const OUTBOUND_BUFFER: usize = 256;
/// Backoff schedule (in ms) for reconnect attempts. The last value is
/// reused for any further attempts so the cap is `10s`.
const RECONNECT_BACKOFF_MS: &[u64] = &[100, 200, 500, 1_000, 2_000, 5_000, 10_000];
/// Floor for the per-attempt reconnect budget: a small liveness override
/// must not shrink it below what a WAN handshake + session replay needs,
/// or the retry loop would livelock aborting every attempt at the bound.
const RECONNECT_ATTEMPT_MIN_BUDGET: Duration = Duration::from_secs(30);
/// Per-attempt reconnect budget: the liveness deadline, floored so liveness
/// tuning bounds detection, not connection establishment.
fn reconnect_attempt_budget(liveness_deadline: Duration) -> Duration {
    liveness_deadline.max(RECONNECT_ATTEMPT_MIN_BUDGET)
}
/// Default WebSocket keepalive ping cadence when a connection does not
/// override [`ConnectionTuning::ws_ping_interval`].
const DEFAULT_WS_PING_INTERVAL: Duration = Duration::from_secs(30);
const SERVE_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);
const SERVE_MAX_ATTEMPTS: u32 = 3;
const CLOCK_PROBE_INTERVAL: Duration = Duration::from_secs(5);
const CLOCK_JUMP_ACCUM_MIN_MS: u64 = 100;
const CLOCK_JUMP_REPORT_MIN_MS: u64 = 2_000;
type WriteErrorSlot = Arc<parking_lot::Mutex<Option<String>>>;
struct HealthState {
    last_inbound: Instant,
    mono_ref: Instant,
    wall_ref: SystemTime,
    clock_jump_accum_ms: u64,
}
struct HealthSnapshot {
    last_inbound: Instant,
    /// Monotonic time elapsed since the last probe window rolled (the most
    /// recent inbound frame or 5s clock probe) — NOT since connection start.
    /// Healthy traffic keeps this small (<= ~5s); the meaningful freeze
    /// signal in this snapshot is `clock_jump_ms`.
    since_last_probe_monotonic_ms: u64,
    /// Wall-clock time elapsed over the same probe window as
    /// `since_last_probe_monotonic_ms`.
    since_last_probe_wall_ms: u64,
    clock_jump_ms: u64,
}
struct ConnHealth {
    state: parking_lot::Mutex<HealthState>,
}
impl ConnHealth {
    fn new() -> Self {
        Self {
            state: parking_lot::Mutex::new(Self::fresh_state()),
        }
    }
    fn fresh_state() -> HealthState {
        HealthState {
            last_inbound: Instant::now(),
            mono_ref: Instant::now(),
            wall_ref: SystemTime::now(),
            clock_jump_accum_ms: 0,
        }
    }
    fn deltas(state: &HealthState) -> (u64, u64) {
        let mono_ms = state.mono_ref.elapsed().as_millis() as u64;
        let wall_ms = SystemTime::now()
            .duration_since(state.wall_ref)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        (mono_ms, wall_ms)
    }
    fn roll(state: &mut HealthState) {
        let (mono_ms, wall_ms) = Self::deltas(state);
        let excess = wall_ms.saturating_sub(mono_ms);
        if excess >= CLOCK_JUMP_ACCUM_MIN_MS {
            state.clock_jump_accum_ms = state.clock_jump_accum_ms.saturating_add(excess);
        }
        state.mono_ref = Instant::now();
        state.wall_ref = SystemTime::now();
    }
    fn record_inbound(&self) {
        let mut state = self.state.lock();
        Self::roll(&mut state);
        state.last_inbound = Instant::now();
    }
    fn refresh_clock(&self) {
        let mut state = self.state.lock();
        Self::roll(&mut state);
    }
    fn snapshot(&self) -> HealthSnapshot {
        let state = self.state.lock();
        let (mono_ms, wall_ms) = Self::deltas(&state);
        let excess = wall_ms.saturating_sub(mono_ms);
        let total =
            state
                .clock_jump_accum_ms
                .saturating_add(if excess >= CLOCK_JUMP_ACCUM_MIN_MS {
                    excess
                } else {
                    0
                });
        HealthSnapshot {
            last_inbound: state.last_inbound,
            since_last_probe_monotonic_ms: mono_ms,
            since_last_probe_wall_ms: wall_ms,
            clock_jump_ms: if total >= CLOCK_JUMP_REPORT_MIN_MS {
                total
            } else {
                0
            },
        }
    }
    fn reset(&self) {
        *self.state.lock() = Self::fresh_state();
    }
}
enum DisconnectCause {
    CloseFrame(Option<u16>),
    Eof,
    ReadError(String),
    WriteError(String),
    Forced,
    /// No inbound frame arrived within the inbound-liveness deadline, so the
    /// transport is silently dead (snapshot-restored VM, NAT/LB flow expiry).
    LivenessDeadline,
}
impl DisconnectCause {
    fn label(&self) -> &'static str {
        match self {
            Self::CloseFrame(_) => "close_frame",
            Self::Eof => "eof",
            Self::ReadError(_) => "transport_read_error",
            Self::WriteError(_) => "transport_write_error",
            Self::Forced => "forced",
            Self::LivenessDeadline => "liveness_deadline",
        }
    }
    fn close_code(&self) -> Option<u16> {
        match self {
            Self::CloseFrame(code) => *code,
            _ => None,
        }
    }
    fn detail(&self) -> Option<&str> {
        match self {
            Self::ReadError(detail) | Self::WriteError(detail) => Some(detail),
            _ => None,
        }
    }
}
struct OutageInfo {
    cause: DisconnectCause,
    prev_connection_id: Option<ConnectionId>,
    prev_connection_duration_ms: u64,
    last_inbound: Instant,
    detect_ms: u64,
    since_last_probe_monotonic_ms: u64,
    since_last_probe_wall_ms: u64,
    clock_jump_ms: u64,
}
enum DeadlineCallError {
    TimedOut(Duration),
    Other(ClientError),
}
impl From<DeadlineCallError> for ClientError {
    fn from(err: DeadlineCallError) -> Self {
        match err {
            DeadlineCallError::TimedOut(timeout) => {
                ClientError::NetworkError(format!("request timed out after {timeout:?}"))
            }
            DeadlineCallError::Other(e) => e,
        }
    }
}
struct WaiterGuard<'a> {
    demux: &'a Demux,
    request_id: &'a xai_tool_protocol::RequestId,
}
impl Drop for WaiterGuard<'_> {
    fn drop(&mut self) {
        let _ = self.demux.take_response_waiter(self.request_id);
    }
}
/// Process-wide default reconnect schedule, materialised once from
/// [`RECONNECT_BACKOFF_MS`]. Connections that do not override
/// [`ConnectionTuning::reconnect_backoff`] share this `Arc` (cheap clone,
/// no per-connect allocation).
fn default_reconnect_backoff() -> Arc<[Duration]> {
    static DEFAULT: std::sync::OnceLock<Arc<[Duration]>> = std::sync::OnceLock::new();
    DEFAULT
        .get_or_init(|| {
            RECONNECT_BACKOFF_MS
                .iter()
                .map(|&ms| Duration::from_millis(ms))
                .collect()
        })
        .clone()
}
/// Resolve a configured backoff schedule, falling back to the built-in
/// table when unset (or empty, which would be degenerate).
fn resolve_reconnect_backoff(configured: Option<Arc<[Duration]>>) -> Arc<[Duration]> {
    match configured {
        Some(schedule) if !schedule.is_empty() => schedule,
        _ => default_reconnect_backoff(),
    }
}
/// Resolve the keepalive ping cadence, clamping an unset *or zero* value to
/// [`DEFAULT_WS_PING_INTERVAL`]. `tokio::time::interval` panics on a zero
/// period, so a configured `Duration::ZERO` (e.g. via
/// `with_ws_ping_interval(0)` or a `StatusConfig.ws_ping` of 0) must never
/// reach the writer task.
fn resolve_ws_ping_interval(configured: Option<Duration>) -> Duration {
    match configured {
        Some(interval) if !interval.is_zero() => interval,
        _ => DEFAULT_WS_PING_INTERVAL,
    }
}
/// Resolve the inbound-liveness deadline, clamping an unset *or zero* value
/// to 2.5× the (already-resolved) keepalive ping cadence — 75s at the
/// default 30s ping.
///
/// The default multiple is chosen for fleet-wide false-positive safety: a
/// healthy connection delivers at least one inbound frame per ping period
/// (the server must answer each WS `Ping` with a `Pong`, and any data frame
/// also counts), so 2.5× tolerates a fully lost/coalesced pong plus
/// scheduling jitter before declaring death. It still detects a silently
/// dead transport (snapshot-restored VM, NAT/LB flow expiry) within ~1–2
/// keepalive cycles instead of TCP-retransmission timescales (15+ min).
/// Explicit overrides are honored verbatim; keep them comfortably above
/// the ping interval or a healthy-but-idle connection will be churned.
fn resolve_ws_liveness_deadline(configured: Option<Duration>, ping_interval: Duration) -> Duration {
    match configured {
        Some(deadline) if !deadline.is_zero() => deadline,
        _ => ping_interval.saturating_mul(5) / 2,
    }
}
/// Optional, default-preserving connection-tuning knobs carried from the
/// pool/builder into [`ConnectionConfig`]. `Default` leaves every value
/// `None`, reproducing the historical hardcoded behaviour — and lets
/// config constructors write `tuning: ConnectionTuning::default()` so new
/// knobs never churn every [`ConnectionConfig`] literal.
#[derive(Clone, Default)]
pub struct ConnectionTuning {
    /// Override for the keepalive ping cadence. `None` (or zero) ⇒
    /// [`DEFAULT_WS_PING_INTERVAL`].
    pub ws_ping_interval: Option<Duration>,
    /// Override for the inbound-liveness deadline: with no inbound frame of
    /// any kind for this long, the reader declares the socket dead and
    /// reconnects. `None` (or zero) ⇒ 2.5× the effective ping cadence (see
    /// [`resolve_ws_liveness_deadline`]).
    pub ws_liveness_deadline: Option<Duration>,
    /// Override for the reconnect backoff schedule. `None` (or empty) ⇒
    /// the built-in [`RECONNECT_BACKOFF_MS`] table. Stored as
    /// `Arc<[Duration]>` so it is shared (not deep-copied) per reconnect.
    pub reconnect_backoff: Option<Arc<[Duration]>>,
}
/// Pool dedup key. Two connections are pooled together iff their
/// `(url, principal)` match.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ConnKey {
    /// Normalised URL (parsed by [`Url::parse`]).
    pub url: String,
    /// Principal projection of the [`AuthCredential`].
    pub principal: PrincipalKey,
}
impl std::fmt::Debug for ConnKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnKey")
            .field("url", &self.url)
            .field("principal", &self.principal)
            .finish()
    }
}
/// Reconnect-callback payload. Dispatched once per successful reconnect
/// so consumers can record metrics or surface UI hints.
#[derive(Debug, Clone)]
pub struct ReconnectEvent {
    /// Server-issued connection id of the FRESH connection (different
    /// from the dropped one).
    pub connection_id: ConnectionId,
    /// Number of session bindings replayed.
    pub sessions_replayed: usize,
    /// Reconnect attempt index (1 for the first reconnect).
    pub attempt: u32,
}
/// Boxed reconnect callback.
pub type ReconnectCallback = Box<dyn Fn(ReconnectEvent) + Send + Sync + 'static>;
/// Boxed disconnect callback, fired when the live socket drops (before a
/// reconnect attempt) and on a terminal close.
pub type DisconnectCallback = Box<dyn Fn() + Send + Sync + 'static>;
/// Boxed connect callback, fired once on the initial successful connect,
/// before the reader actor task spawns. It therefore strictly happens-before
/// any disconnect/reconnect callback, so a connect/disconnect pair can never
/// be observed out of order (e.g. a readiness marker resurrected after the
/// socket has already dropped).
pub type ConnectCallback = Box<dyn Fn() + Send + Sync + 'static>;
/// A live (or reconnecting) connection to the server.
///
/// Cheap to clone via the `Arc` returned from
/// [`crate::HubConnectionPool::get_or_connect`]. Methods on the inner
/// `HubConnection` are `&self` so multiple consumers can share the
/// same instance without external locking.
///
/// Dropping the last `Arc<HubConnection>` runs [`Drop`], which sends
/// a stop signal to the connection actor; the actor drains every
/// in-flight response waiter with [`ClientError::NetworkError`] and
/// exits asynchronously. [`Self::request_shutdown`] triggers the
/// same stop-and-drain sequence without giving up the `Arc`.
pub struct HubConnection {
    inner: Arc<HubConnectionInner>,
}
impl std::fmt::Debug for HubConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubConnection")
            .field("key", &self.inner.key)
            .field("kind", &self.inner.kind)
            .finish_non_exhaustive()
    }
}
/// Configuration for a [`HubConnection`].
///
/// Consumed by [`HubConnection::connect`]; not `Clone` because the
/// only path that wants a copy is the pool, and the pool builds a
/// fresh config per attempt rather than cloning.
pub struct ConnectionConfig {
    /// `ws://` or `wss://` URL of the server.
    pub url: Url,
    pub credential: Arc<dyn AuthProvider>,
    /// Connection role announced in the hello frame.
    pub kind: ConnectionKind,
    /// Optional reconnect-event callback.
    pub on_reconnect: Option<Arc<ReconnectCallback>>,
    /// Optional disconnect callback, fired when the live socket drops or the
    /// server sends a terminal close.
    pub on_disconnect: Option<Arc<DisconnectCallback>>,
    /// Optional connect callback, fired once on the initial successful connect
    /// before the actor starts (so it happens-before any disconnect/reconnect).
    pub on_connect: Option<Arc<ConnectCallback>>,
    /// Stable server identity sent in the hello frame. Only meaningful
    /// for [`ConnectionKind::ToolServer`] connections.
    pub server_id: Option<xai_tool_protocol::ServerId>,
    /// One-line server description for `servers.list`.
    pub server_description: Option<String>,
    /// Opaque metadata surfaced in `ServerInfo.metadata`.
    pub server_metadata: Option<serde_json::Value>,
    /// Optional override for the outbound mpsc bound. `None` uses the
    /// crate default (matched to the server's per-actor outbound
    /// buffer). Tests use this to exercise the
    /// bounded-wait fast-fail path without flooding production-sized
    /// buffers.
    pub outbound_buffer: Option<usize>,
    /// Optional tuning knobs (ping cadence, liveness deadline, reconnect
    /// backoff). `ConnectionTuning::default()` keeps every historical
    /// default.
    pub tuning: ConnectionTuning,
    /// When set, attached as an extra access header on every
    /// (re)connect, unconditionally. Harmless when the peer ignores it.
    pub alpha_test_key: Option<String>,
    /// Allow a plaintext `ws://` connection to a non-loopback host.
    /// Only enable when the transport is otherwise secured (e.g. a
    /// private network or TLS-terminating proxy) — otherwise the bearer
    /// credential crosses the network in cleartext.
    pub allow_insecure_ws: bool,
    /// Optional weak handle to the owning pool, set by
    /// [`crate::HubConnectionPool::get_or_connect`]. On a fatal
    /// handshake-auth failure the reconnect driver evicts its own pool
    /// entry through this so the next caller opens a fresh socket.
    /// `None` for the unpooled [`HubConnection::connect`] path (tests /
    /// one-shot) — nothing to evict. Weak so the pool↔connection edge
    /// is not an ownership cycle.
    pub on_fatal: Option<Weak<crate::pool::HubConnectionPool>>,
}
impl std::fmt::Debug for ConnectionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionConfig")
            .field("url", &self.url.as_str())
            .field("credential", &self.credential)
            .field("kind", &self.kind)
            .field("allow_insecure_ws", &self.allow_insecure_ws)
            .field("on_reconnect", &self.on_reconnect.is_some())
            .finish()
    }
}
struct HubConnectionInner {
    key: ConnKey,
    kind: ConnectionKind,
    credential: Arc<dyn AuthProvider>,
    on_reconnect: Option<Arc<ReconnectCallback>>,
    on_disconnect: Option<Arc<DisconnectCallback>>,
    server_id: Option<xai_tool_protocol::ServerId>,
    server_description: Option<String>,
    server_metadata: Option<serde_json::Value>,
    /// Attached as an extra access header on every (re)connect when set.
    alpha_test_key: Option<String>,
    /// Permit plaintext `ws://` to a non-loopback host (transport otherwise secured).
    allow_insecure_ws: bool,
    /// See [`ConnectionConfig::on_fatal`].
    on_fatal: Option<Weak<crate::pool::HubConnectionPool>>,
    /// Resolved reconnect backoff schedule (configured override or the
    /// built-in table). Resolved once at connect; shared per reconnect.
    reconnect_backoff: Arc<[Duration]>,
    /// Outbound frames waiting to be written. Filled by `send_*`
    /// helpers; drained by the writer half of the actor.
    outbound_tx: mpsc::Sender<String>,
    /// Inbound demux state (response waiters + session inboxes).
    demux: Arc<Demux>,
    /// Refcounted bound-session set. Used by the reconnect path to
    /// re-issue `register_session` for every still-live session.
    bound_sessions: Arc<RefCountedSet<SessionId>>,
    /// Cached server-issued `connection_id`. Updated on every (re)connect.
    connection_id: Arc<Mutex<Option<ConnectionId>>>,
    /// Optional capabilities the server advertised in the most recent
    /// `hello_ack` (wire method strings). Refreshed on every (re)connect
    /// handshake. Empty when the ack carried none — on the wire that is
    /// indistinguishable from a server predating the field, so
    /// [`HubConnection::supports`] reports unknown in that case.
    hello_capabilities: parking_lot::RwLock<Vec<String>>,
    /// Monotonically-increasing JSON-RPC request id counter.
    next_request_id: std::sync::atomic::AtomicU64,
    /// Cancelled by the actor task once it has fully exited so
    /// `await_shutdown` resolves promptly. `CancellationToken` has
    /// persistent semantics so a wait that arrives AFTER the actor
    /// has already cancelled the token still wakes immediately.
    shutdown: CancellationToken,
    /// Stops the actor on `Drop`.
    stop_tx: mpsc::Sender<()>,
    reconnect_tx: mpsc::Sender<()>,
    early_notif_rx: parking_lot::Mutex<Option<broadcast::Receiver<Value>>>,
    health: ConnHealth,
    writer_error: WriteErrorSlot,
}
type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
impl HubConnection {
    /// Open a brand-new [`HubConnection`] and spawn its actor task.
    ///
    /// The pool is the canonical caller; outside callers MAY use this
    /// for tests or one-shot programs but lose pool dedup.
    pub async fn connect(config: ConnectionConfig) -> Result<Arc<Self>, ClientError> {
        let initial_cred = config.credential.current();
        let key = ConnKey {
            url: config.url.as_str().to_owned(),
            principal: config.credential.principal_key(),
        };
        let ws_ping_interval = resolve_ws_ping_interval(config.tuning.ws_ping_interval);
        let ws_liveness_deadline =
            resolve_ws_liveness_deadline(config.tuning.ws_liveness_deadline, ws_ping_interval);
        if ws_liveness_deadline <= ws_ping_interval {
            warn!(
                ?ws_liveness_deadline,
                ?ws_ping_interval,
                "ws liveness deadline is not greater than the keepalive ping interval; healthy idle connections will be killed and reconnected every window"
            );
        }
        let reconnect_backoff = resolve_reconnect_backoff(config.tuning.reconnect_backoff);
        let buffer = config.outbound_buffer.unwrap_or(OUTBOUND_BUFFER);
        let (outbound_tx, outbound_rx) = mpsc::channel::<String>(buffer);
        let (stop_tx, stop_rx) = mpsc::channel::<()>(1);
        let (reconnect_tx, reconnect_rx) = mpsc::channel::<()>(1);
        let demux = Arc::new(Demux::with_outbound(outbound_tx.clone()));
        let bound_sessions = Arc::new(RefCountedSet::<SessionId>::new());
        let connection_id = Arc::new(Mutex::new(None));
        let shutdown = CancellationToken::new();
        let ws = open_socket(
            &config.url,
            &initial_cred,
            config.kind,
            config.alpha_test_key.as_deref(),
            config.allow_insecure_ws,
        )
        .await?;
        let (sink, stream) = ws.split();
        let (sink, stream, ack) = run_handshake(
            sink,
            stream,
            config.kind,
            config.server_id.clone(),
            config.server_description.clone(),
            config.server_metadata.clone(),
        )
        .await?;
        *connection_id.lock().await = Some(ack.connection_id.clone());
        info!(
            url = %config.url,
            connection_id = %ack.connection_id,
            "server connection established"
        );
        if let Some(cb) = &config.on_connect {
            cb();
        }
        let early_notif_rx = parking_lot::Mutex::new(match config.kind {
            ConnectionKind::ToolServer => Some(demux.subscribe_notifications()),
            _ => None,
        });
        let writer_error: WriteErrorSlot = Arc::new(parking_lot::Mutex::new(None));
        let inner = Arc::new(HubConnectionInner {
            key,
            kind: config.kind,
            credential: config.credential,
            on_reconnect: config.on_reconnect.clone(),
            on_disconnect: config.on_disconnect.clone(),
            server_id: config.server_id,
            server_description: config.server_description,
            server_metadata: config.server_metadata,
            alpha_test_key: config.alpha_test_key,
            allow_insecure_ws: config.allow_insecure_ws,
            on_fatal: config.on_fatal,
            reconnect_backoff,
            outbound_tx,
            demux: demux.clone(),
            bound_sessions: bound_sessions.clone(),
            connection_id,
            hello_capabilities: parking_lot::RwLock::new(ack.capabilities),
            next_request_id: std::sync::atomic::AtomicU64::new(1),
            shutdown,
            stop_tx,
            reconnect_tx,
            early_notif_rx,
            health: ConnHealth::new(),
            writer_error: writer_error.clone(),
        });
        let (writer_ctl_tx, writer_ctl_rx) =
            mpsc::channel::<WriterControl<SplitSink<WsStream, Message>>>(2);
        let (writer_stop_tx, writer_stop_rx) = mpsc::channel::<()>(1);
        let writer_handle = tokio::spawn(run_writer(
            sink,
            outbound_rx,
            writer_ctl_rx,
            writer_stop_rx,
            ws_ping_interval,
            writer_error,
        ));
        let reader_inner = inner.clone();
        tokio::spawn(run_reader_actor(
            reader_inner,
            stream,
            stop_rx,
            reconnect_rx,
            writer_ctl_tx,
            writer_stop_tx,
            writer_handle,
            config.url,
            ws_liveness_deadline,
        ));
        Ok(Arc::new(Self { inner }))
    }
    /// Pool dedup key for this connection.
    pub fn key(&self) -> &ConnKey {
        &self.inner.key
    }
    /// Connection role.
    pub fn kind(&self) -> ConnectionKind {
        self.inner.kind
    }
    /// Stable identity of this connection's actor state. Lets the pool
    /// evict by identity so a connection only ever forgets its own slot.
    pub(crate) fn actor_id(&self) -> usize {
        Arc::as_ptr(&self.inner) as *const () as usize
    }
    /// Server-issued connection id of the most recently established
    /// (post-handshake) socket. During a reconnect gap this still names the
    /// dropped connection until the next handshake + replay completes.
    pub async fn connection_id(&self) -> Option<ConnectionId> {
        self.inner.connection_id.lock().await.clone()
    }
    /// Whether the server advertised `capability` (a wire method string,
    /// e.g. `"session_attach_server"`) in the CURRENT connection's
    /// `hello_ack`.
    ///
    /// - `Some(true)`: advertised.
    /// - `Some(false)`: the ack carried a non-empty capability list that
    ///   does not include it.
    /// - `None`: the ack advertised nothing — servers predating the
    ///   `capabilities` field are indistinguishable from an empty list, so
    ///   support is unknown and callers should probe per call.
    pub fn supports(&self, capability: &str) -> Option<bool> {
        let caps = self.inner.hello_capabilities.read();
        if caps.is_empty() {
            return None;
        }
        Some(caps.iter().any(|c| c == capability))
    }
    /// Demux (used by the server-side run loop to register session
    /// inboxes). Cheap to clone (Arc bump).
    pub fn demux(&self) -> Arc<Demux> {
        self.inner.demux.clone()
    }
    pub(crate) fn take_early_notifications(&self) -> Option<broadcast::Receiver<Value>> {
        self.inner.early_notif_rx.lock().take()
    }
    pub(crate) fn force_reconnect(&self) {
        let _ = self.inner.reconnect_tx.try_send(());
    }
    /// Future that resolves once the connection actor has shut down.
    pub async fn await_shutdown(&self) {
        self.inner.shutdown.cancelled().await;
    }
    /// Signal the connection actor to begin shutdown. The actor
    /// drains its in-flight waiters with `NetworkError` and exits;
    /// the outbound channel closes shortly after, so subsequent
    /// [`Self::send_outbound`] calls return
    /// [`ClientError::NetworkError`]. [`Self::await_shutdown`]
    /// resolves once the actor task has terminated.
    ///
    /// Idempotent: redundant calls are no-ops. Equivalent to
    /// dropping the last `Arc<HubConnection>`, but lets a holder
    /// trigger shutdown without giving up its reference.
    pub fn request_shutdown(&self) {
        let _ = self.inner.stop_tx.try_send(());
    }
    /// Increment the refcount on `session_id`. The session is tracked
    /// locally for reconnect-replay; the server learns about it via
    /// `serve` (auto-registration on the server side).
    pub fn track_session(&self, session_id: SessionId) {
        self.inner.bound_sessions.increment(session_id);
    }
    /// Decrement the refcount on `session_id`. Removes tracking when
    /// the last borrower drops. Returns the post-decrement count
    /// (`Some(0)` = last borrower; `None` = key was absent).
    pub fn untrack_session(&self, session_id: &SessionId) -> Option<u64> {
        self.inner.bound_sessions.decrement(session_id)
    }
    /// Send a JSON-RPC request and await the response.
    ///
    /// The waiter is registered before the frame is sent so a fast response
    /// can never arrive before its waiter exists, and is reclaimed on send
    /// failure (via [`WaiterGuard`]) so a call that never reached the wire
    /// cannot leak a parked waiter across a reconnect episode.
    pub async fn call_request<P>(
        &self,
        request_id: xai_tool_protocol::RequestId,
        request: &JsonRpcRequest<P>,
    ) -> Result<JsonRpcResponse, ClientError>
    where
        P: serde::Serialize,
    {
        let text = serde_json::to_string(request)?;
        let (tx, rx) = oneshot::channel();
        self.inner
            .demux
            .register_response_waiter(request_id.clone(), tx);
        let _guard = WaiterGuard {
            demux: &self.inner.demux,
            request_id: &request_id,
        };
        self.send_outbound(text).await?;
        rx.await?
    }
    /// Send a JSON-RPC request and await the response, bounded by `timeout`.
    pub async fn call_request_with_timeout<P>(
        &self,
        request_id: xai_tool_protocol::RequestId,
        request: &JsonRpcRequest<P>,
        timeout: Duration,
    ) -> Result<JsonRpcResponse, ClientError>
    where
        P: serde::Serialize,
    {
        self.call_request_with_deadline(request_id, request, timeout)
            .await
            .map_err(ClientError::from)
    }
    async fn call_request_with_deadline<P>(
        &self,
        request_id: xai_tool_protocol::RequestId,
        request: &JsonRpcRequest<P>,
        timeout: Duration,
    ) -> Result<JsonRpcResponse, DeadlineCallError>
    where
        P: serde::Serialize,
    {
        let text =
            serde_json::to_string(request).map_err(|e| DeadlineCallError::Other(e.into()))?;
        let (tx, rx) = oneshot::channel();
        self.inner
            .demux
            .register_response_waiter(request_id.clone(), tx);
        let _guard = WaiterGuard {
            demux: &self.inner.demux,
            request_id: &request_id,
        };
        self.send_outbound(text)
            .await
            .map_err(DeadlineCallError::Other)?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result.map_err(DeadlineCallError::Other),
            Ok(Err(recv_err)) => Err(DeadlineCallError::Other(recv_err.into())),
            Err(_elapsed) => Err(DeadlineCallError::TimedOut(timeout)),
        }
    }
    /// Send a fully-formed JSON text frame onto the outbound channel.
    /// Used by the server-side handler when replying to a
    /// `tool_call_request` (the response flows out without going
    /// through a waiter).
    pub async fn send_outbound(&self, text: String) -> Result<(), ClientError> {
        match self.inner.outbound_tx.try_send(text) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(text)) => {
                match tokio::time::timeout(
                    Duration::from_millis(250),
                    self.inner.outbound_tx.send(text),
                )
                .await
                {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(_)) => Err(ClientError::NetworkError(
                        "outbound channel closed".to_owned(),
                    )),
                    Err(_) => Err(ClientError::BackpressureError(
                        "outbound mpsc full beyond bounded wait".to_owned(),
                    )),
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(ClientError::NetworkError(
                "outbound channel closed".to_owned(),
            )),
        }
    }
    /// Non-blocking enqueue for synchronous drop paths that cannot
    /// `.await` (e.g. `RemoteCallStream::Drop` cancel-on-drop). A full
    /// or closed channel returns `Err` and the caller abandons the
    /// frame — best-effort, mirroring the heartbeat-pong drop discipline.
    pub(crate) fn try_send_outbound(&self, text: String) -> Result<(), ClientError> {
        match self.inner.outbound_tx.try_send(text) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(ClientError::BackpressureError(
                "outbound mpsc full".to_owned(),
            )),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(ClientError::NetworkError(
                "outbound channel closed".to_owned(),
            )),
        }
    }
    /// Allocate a fresh request id. Monotonic per-connection.
    ///
    /// Returns `Err` only if a future-added `RequestId` invariant
    /// rejects the formatted `c{value}` string (today the only
    /// failure path is the empty-string check, which `format!` cannot
    /// produce). Callers in non-fallible contexts should propagate
    /// the error rather than panic.
    pub fn try_alloc_request_id(&self) -> Result<xai_tool_protocol::RequestId, ClientError> {
        let value = self
            .inner
            .next_request_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        xai_tool_protocol::RequestId::new(format!("c{value}")).map_err(ClientError::from)
    }
    /// Number of sessions currently bound to this connection.
    /// Stable observable for monitoring and tests; not on the hot path.
    pub fn bound_session_count(&self) -> usize {
        self.inner.bound_sessions.len()
    }
    /// Send a `serve` frame: full tool snapshot for a session.
    ///
    /// Idempotent: re-sending replaces the tool set. The server diffs
    /// against the previous snapshot and emits `tools_changed` to
    /// subscribed harnesses.
    pub async fn serve(
        &self,
        session_id: SessionId,
        params: xai_tool_protocol::ServeParams,
    ) -> Result<xai_tool_protocol::ServeResult, ClientError> {
        let mut last_err: Option<ClientError> = None;
        for attempt in 1..=SERVE_MAX_ATTEMPTS {
            let request_id = self.try_alloc_request_id()?;
            let req = JsonRpcRequest {
                jsonrpc: JsonRpcVersion,
                id: JsonRpcId::from_request_id(&request_id),
                session_id: Some(session_id.clone()),
                method: Method::Serve.as_wire_str().to_owned(),
                params: &params,
            };
            match self
                .call_request_with_deadline(request_id, &req, SERVE_ATTEMPT_TIMEOUT)
                .await
            {
                Ok(resp) => {
                    return match resp.outcome {
                        ResponseOutcome::Result(value) => serde_json::from_value(value)
                            .map_err(|e| ClientError::Serde(e.to_string())),
                        ResponseOutcome::Error(err) => Err(ClientError::from_jsonrpc_error(err)),
                    };
                }
                Err(DeadlineCallError::TimedOut(timeout)) => {
                    crate::metrics::serve_replay_timeout();
                    warn!(%session_id, attempt, ?timeout, "serve attempt timed out; will retry");
                    last_err = Some(DeadlineCallError::TimedOut(timeout).into());
                }
                Err(DeadlineCallError::Other(e)) => return Err(e),
            }
        }
        warn!(
            %session_id,
            attempts = SERVE_MAX_ATTEMPTS,
            "serve timed out every bounded attempt; forcing reconnect to restart replay"
        );
        self.force_reconnect();
        Err(last_err.unwrap_or_else(|| {
            ClientError::NetworkError("serve failed after bounded retries".to_owned())
        }))
    }
}
impl Drop for HubConnection {
    fn drop(&mut self) {
        let _ = self.inner.stop_tx.try_send(());
    }
}
/// True iff `url`'s host is one of the canonical loopback names. Case
/// insensitive on the hostname; IP literals match the standard loopback
/// addresses for IPv4 and IPv6.
pub(crate) fn host_is_loopback(url: &Url) -> bool {
    use std::net::{Ipv4Addr, Ipv6Addr};
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip == Ipv4Addr::LOCALHOST,
        Some(url::Host::Ipv6(ip)) => ip == Ipv6Addr::LOCALHOST,
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}
/// Open a fresh `ws://` / `wss://` socket. No handshake yet.
///
/// Refuses to send the credential over `ws://` to any non-loopback host
/// so the bearer token never crosses the network in plaintext. Local
/// loopback (`127.0.0.1`, `::1`, `localhost`) is the explicit exception
/// for development and local-proxy use; every other host must be
/// reached over `wss://`.
async fn open_socket(
    url: &Url,
    credential: &AuthCredential,
    kind: ConnectionKind,
    alpha_test_key: Option<&str>,
    allow_insecure_ws: bool,
) -> Result<WsStream, ClientError> {
    let is_plaintext_remote = url.scheme() != "wss" && !host_is_loopback(url);
    if is_plaintext_remote && !allow_insecure_ws {
        return Err(ClientError::InsecureScheme { url: url.clone() });
    }
    if is_plaintext_remote {
        warn!(
            host = %url.host_str().unwrap_or(""),
            "opening server connection over plaintext ws:// (allow_insecure_ws=true); bearer crosses the network in cleartext"
        );
    }
    let mut connect_url = url.clone();
    let expected_role = match kind {
        ConnectionKind::Harness => "harness",
        ConnectionKind::ToolServer => "tool_server",
    };
    if let Some(existing) = connect_url
        .query_pairs()
        .find(|(k, _)| k == "role")
        .map(|(_, v)| v.to_string())
    {
        if existing != expected_role {
            return Err(ClientError::InvalidConfig(format!(
                "URL query parameter role={existing} conflicts with ConnectionKind::{kind:?} (expected role={expected_role})"
            )));
        }
    } else {
        connect_url
            .query_pairs_mut()
            .append_pair("role", expected_role);
    }
    let mut request = connect_url
        .as_str()
        .into_client_request()
        .map_err(|e| ClientError::InvalidConfig(format!("invalid ws request: {e}")))?;
    let headers = request.headers_mut();
    for (name, value) in credential.upgrade_headers() {
        let header_name: HeaderName = name;
        let header_value: HeaderValue = HeaderValue::from_str(&value)
            .map_err(|e| ClientError::InvalidConfig(format!("invalid auth header value: {e}")))?;
        headers.insert(header_name, header_value);
    }
    let _ = alpha_test_key;
    xai_tracing::http_client::attach_trace_to_http_request(headers);
    let (ws, _resp) = connect_async(request)
        .await
        .map_err(ClientError::from_handshake_error)?;
    Ok(ws)
}
/// Drive the hello / hello_ack exchange and hand back the (sink,
/// stream) pair for steady-state use.
async fn run_handshake(
    mut sink: SplitSink<WsStream, Message>,
    mut stream: SplitStream<WsStream>,
    kind: ConnectionKind,
    server_id: Option<xai_tool_protocol::ServerId>,
    server_description: Option<String>,
    server_metadata: Option<serde_json::Value>,
) -> Result<
    (
        SplitSink<WsStream, Message>,
        SplitStream<WsStream>,
        xai_tool_protocol::HelloAckMsg,
    ),
    ClientError,
> {
    let ack = send_hello(
        &mut sink,
        &mut stream,
        kind,
        server_id,
        server_description,
        server_metadata,
    )
    .await?;
    Ok((sink, stream, ack))
}
/// Outcome of the connected-phase loop.
enum ConnectedExit {
    /// Stop signal — actor terminates.
    Stop,
    /// Socket closed / errored — actor enters reconnect.
    SocketClosed(DisconnectCause),
    /// Server sent a close frame with a code that means "do not reconnect"
    /// (e.g. force eviction, session expired, admin disconnect).
    TerminalClose(u16),
}
/// Current Unix time in milliseconds (saturating to 0 before the epoch).
fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
/// Decode an inbound text frame. Returns the serialized [`PongFrame`]
/// to send back when the frame is an app-level server `ping`; otherwise
/// routes the frame through the demux and returns `None`.
fn route_or_pong(inner: &HubConnectionInner, text: &str) -> Option<String> {
    match serde_json::from_str::<Value>(text) {
        Ok(value) => {
            if value.get("method").and_then(Value::as_str) == Some(Method::Ping.as_wire_str()) {
                serde_json::to_string(&PongFrame::new(now_unix_millis())).ok()
            } else {
                let _ = inner.demux.route(value);
                None
            }
        }
        Err(e) => {
            warn!(?e, "discarding unparseable inbound text frame");
            None
        }
    }
}
/// Map a websocket close frame's code to the connected-phase exit. Close
/// codes 4100-4199 are terminal (the server intentionally ended the
/// connection: eviction, session expiry, admin disconnect, rate limit).
/// The range is deliberately wide so new terminal codes added server-side
/// are recognised without a client update.
fn exit_for_close_code(code: Option<u16>) -> ConnectedExit {
    match code {
        Some(code) if (4100..4200).contains(&code) => ConnectedExit::TerminalClose(code),
        _ => ConnectedExit::SocketClosed(DisconnectCause::CloseFrame(code)),
    }
}
/// Classify why the inbound stream ended, preferring a write error the
/// writer task recorded over what the reader observed.
///
/// Best-effort: the writer task populates `writer_error` asynchronously
/// after its send fails, so the reader can observe the resulting stream
/// EOF/error and classify it here *before* the slot is set. In that
/// (telemetry-only) race a genuine write-side failure is reported as
/// `eof` / `transport_read_error` instead of `transport_write_error`.
fn classify_stream_end(inner: &HubConnectionInner, read_error: Option<String>) -> DisconnectCause {
    if let Some(detail) = inner.writer_error.lock().take() {
        return DisconnectCause::WriteError(detail);
    }
    match read_error {
        Some(detail) => DisconnectCause::ReadError(detail),
        None => DisconnectCause::Eof,
    }
}
/// Control messages handed to the dedicated writer task.
///
/// The reader is the sole reconnect driver; it `Pause`s the writer the
/// instant the socket is known dead so no buffered frame is dequeued
/// onto the corpse, then `Resume`s it with the fresh sink once the
/// handshake completes. Carried on a cap-2 channel so a `Pause` is never
/// dropped.
enum WriterControl<S> {
    /// Socket is dead; stop draining `outbound_rx` (frames stay buffered).
    Pause,
    /// Reconnected; install the fresh sink and resume draining.
    Resume(S),
}
/// Dedicated writer task: owns the sink, drains `outbound_rx`, and fires
/// the keepalive ping (`ping_period`) — but only while `live`. Between a `Pause` and
/// the matching `Resume` it parks on the control/stop channels only, so
/// frames enqueued during the reconnect gap stay buffered in
/// `outbound_rx` and flush after `Resume` (no multi-frame loss; the
/// single in-flight frame whose `send` fails is the only loss, matching
/// the pre-split worst case).
///
/// Generic over the sink so it can be unit-tested with an in-memory sink
/// without a live socket.
async fn run_writer<S>(
    mut sink: S,
    mut outbound_rx: mpsc::Receiver<String>,
    mut writer_ctl_rx: mpsc::Receiver<WriterControl<S>>,
    mut writer_stop_rx: mpsc::Receiver<()>,
    ping_period: Duration,
    write_error: WriteErrorSlot,
) where
    S: futures::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    let mut ping_interval = tokio::time::interval(ping_period);
    ping_interval.tick().await;
    let mut live = true;
    loop {
        tokio::select! {
            biased;
            _ = writer_stop_rx.recv() => break,
            ctl = writer_ctl_rx.recv() => match ctl {
                Some(WriterControl::Pause) => live = false,
                Some(WriterControl::Resume(new_sink)) => {
                    sink = new_sink;
                    live = true;
                    // Discard any error a late old-sink send left behind. The
                    // reader clears the slot before sending `Resume`, but an
                    // in-flight send on the dead socket (e.g. blocked on TCP
                    // retransmits since before `Pause`) can fail after that
                    // clear and re-fill the slot. This task is the only slot
                    // writer and processes messages sequentially, so by the
                    // time `Resume` is handled that old-sink send has
                    // finished — clearing here closes the race and stops a
                    // stale detail from mislabeling the NEXT disconnect as
                    // transport_write_error.
                    write_error.lock().take();
                    // Restart the keepalive cadence from the reconnect instant:
                    // consume the immediate first tick so the next ping fires
                    // one period after Resume, not as a catch-up burst for ticks
                    // missed while paused.
                    ping_interval = tokio::time::interval(ping_period);
                    ping_interval.tick().await;
                }
                // Reader gone (control sender dropped) → wind down.
                None => break,
            },
            _ = ping_interval.tick(), if live => {
                if let Err(e) = sink.send(Message::Ping(Vec::new().into())).await {
                    // The reader detects the death (stream error, or liveness-
                    // deadline expiry once pings stop being answered) and
                    // drives the reconnect; we just stop draining onto the
                    // corpse.
                    *write_error.lock() = Some(format!("ping send failed: {e}"));
                    crate::metrics::writer_sink_send_error();
                    live = false;
                }
            }
            outbound = outbound_rx.recv(), if live => match outbound {
                Some(text) => {
                    if let Err(e) = sink.send(Message::Text(text.into())).await {
                        *write_error.lock() = Some(format!("frame send failed: {e}"));
                        crate::metrics::writer_sink_send_error();
                        live = false;
                    }
                }
                // Last `outbound_tx` dropped → channel closed → wind down.
                None => break,
            },
        }
    }
}
/// Invoke the optional disconnect callback (best-effort, sync).
fn fire_on_disconnect(inner: &HubConnectionInner) {
    if let Some(cb) = &inner.on_disconnect {
        cb();
    }
}
/// Reader half of the split actor: owns the stream, routes inbound
/// frames, and drives reconnect. Never touches the sink — it asks the
/// writer task to `Pause`/`Resume` instead.
async fn run_reader_actor(
    inner: Arc<HubConnectionInner>,
    mut stream: SplitStream<WsStream>,
    mut stop_rx: mpsc::Receiver<()>,
    mut reconnect_rx: mpsc::Receiver<()>,
    writer_ctl_tx: mpsc::Sender<WriterControl<SplitSink<WsStream, Message>>>,
    writer_stop_tx: mpsc::Sender<()>,
    writer_handle: tokio::task::JoinHandle<()>,
    url: Url,
    liveness_deadline: Duration,
) {
    let mut attempt: u32 = 0;
    let mut connected_at = Instant::now();
    'actor: loop {
        match run_reader_phase(
            inner.as_ref(),
            &mut stream,
            &mut stop_rx,
            &mut reconnect_rx,
            liveness_deadline,
        )
        .await
        {
            ConnectedExit::Stop => break,
            ConnectedExit::TerminalClose(code) => {
                info!(code, url = %url, "server sent terminal close; not reconnecting");
                fire_on_disconnect(inner.as_ref());
                inner.demux.drain_waiters_with(|| {
                    ClientError::Closed(format!("server terminal close (code {code})"))
                });
                inner.demux.drain_progress();
                break;
            }
            ConnectedExit::SocketClosed(cause) => {
                let detected_at = Instant::now();
                let health = inner.health.snapshot();
                let prev_connection_id = inner.connection_id.lock().await.clone();
                let outage = OutageInfo {
                    prev_connection_id,
                    prev_connection_duration_ms: detected_at
                        .duration_since(connected_at)
                        .as_millis() as u64,
                    last_inbound: health.last_inbound,
                    detect_ms: detected_at.duration_since(health.last_inbound).as_millis() as u64,
                    since_last_probe_monotonic_ms: health.since_last_probe_monotonic_ms,
                    since_last_probe_wall_ms: health.since_last_probe_wall_ms,
                    clock_jump_ms: health.clock_jump_ms,
                    cause,
                };
                warn!(
                    url = %url,
                    cause = outage.cause.label(),
                    close_code = ?outage.cause.close_code(),
                    error_detail = ?outage.cause.detail(),
                    connection_id = ?outage.prev_connection_id,
                    prev_connection_duration_ms = outage.prev_connection_duration_ms,
                    detect_ms = outage.detect_ms,
                    since_last_probe_monotonic_ms = outage.since_last_probe_monotonic_ms,
                    since_last_probe_wall_ms = outage.since_last_probe_wall_ms,
                    clock_jump_ms = outage.clock_jump_ms,
                    "server connection lost; scheduling reconnect"
                );
                fire_on_disconnect(inner.as_ref());
                if writer_ctl_tx.send(WriterControl::Pause).await.is_err() {
                    break;
                }
                inner.demux.drain_waiters_with(|| {
                    ClientError::NetworkError("socket dropped during in-flight call".to_owned())
                });
                inner.demux.drain_progress();
                let mut backoff_total = Duration::ZERO;
                loop {
                    attempt = attempt.saturating_add(1);
                    let backoff = backoff_for(attempt, &inner.reconnect_backoff);
                    info!(?backoff, attempt, url = %url, "reconnecting server connection");
                    tokio::select! {
                        _ = stop_rx.recv() => break 'actor,
                        _ = sleep(backoff) => {}
                    }
                    backoff_total += backoff;
                    let reconnect_start = std::time::Instant::now();
                    let attempt_budget = reconnect_attempt_budget(liveness_deadline);
                    let outcome = tokio::select! {
                        _ = stop_rx.recv() => break 'actor,
                        outcome = tokio::time::timeout(
                            attempt_budget,
                            reconnect_and_replay(
                                inner.as_ref(),
                                &url,
                                attempt,
                                &outage,
                                backoff_total,
                            ),
                        ) => outcome.unwrap_or_else(|_elapsed| {
                            Err(ClientError::NetworkError(format!(
                                "reconnect attempt timed out after {attempt_budget:?}"
                            )))
                        }),
                    };
                    match outcome {
                        Ok((new_sink, new_stream)) => {
                            let elapsed = reconnect_start.elapsed().as_secs_f64();
                            crate::metrics::reconnect_succeeded();
                            crate::metrics::reconnect_duration_observe(elapsed);
                            inner.health.reset();
                            inner.writer_error.lock().take();
                            connected_at = Instant::now();
                            drain_reconnect_signals(&mut reconnect_rx);
                            stream = new_stream;
                            if writer_ctl_tx
                                .send(WriterControl::Resume(new_sink))
                                .await
                                .is_err()
                            {
                                break 'actor;
                            }
                            crate::metrics::reconnect_writer_resume();
                            break;
                        }
                        Err(ClientError::HandshakeAuthFailed { status }) => {
                            warn!(
                                status,
                                attempt,
                                "reconnect rejected with handshake auth failure; evicting pool entry and stopping"
                            );
                            crate::metrics::reconnect_failed("handshake_auth");
                            inner.demux.drain_waiters_with(|| {
                                ClientError::AuthError(format!(
                                    "server rejected reconnect handshake (HTTP {status})"
                                ))
                            });
                            inner.demux.drain_progress();
                            if let Some(pool) = inner.on_fatal.as_ref().and_then(Weak::upgrade) {
                                let own_id = Arc::as_ptr(&inner) as *const () as usize;
                                pool.forget_if(&inner.key, move |conn| conn.actor_id() == own_id);
                            }
                            break 'actor;
                        }
                        Err(err) => {
                            crate::metrics::reconnect_failed("transport");
                            warn!(
                                ?err,
                                attempt,
                                cause = outage.cause.label(),
                                backoff_total_ms = backoff_total.as_millis() as u64,
                                "reconnect attempt failed; will retry"
                            );
                        }
                    }
                }
            }
        }
    }
    inner
        .demux
        .drain_waiters_with(|| ClientError::NetworkError("connection actor exited".to_owned()));
    inner.demux.drain_progress();
    let _ = writer_stop_tx.send(()).await;
    drop(writer_ctl_tx);
    drop(stop_rx);
    drop(stream);
    if let Err(e) = writer_handle.await {
        warn!(?e, "writer task panicked during shutdown");
    }
    inner.shutdown.cancel();
}
fn drain_reconnect_signals(reconnect_rx: &mut mpsc::Receiver<()>) {
    while reconnect_rx.try_recv().is_ok() {}
}
/// Reader-only steady-state loop for the split actor: drives the inbound
/// half but never writes (app-level pongs route through `outbound_tx`; WS
/// pings are auto-answered by tungstenite on poll).
///
/// Enforces the inbound-liveness deadline: no inbound frame of any kind for
/// the deadline window (default 2.5× the ping cadence, see
/// [`resolve_ws_liveness_deadline`]) means the transport is silently dead
/// (snapshot-restored VM, NAT/LB flow expiry), so exit via
/// [`ConnectedExit::SocketClosed`] onto the normal reconnect path. The
/// deadline runs only in this phase and re-arms on every (re)entry.
///
/// Generic over the stream for in-memory unit tests, mirroring
/// [`run_writer`].
async fn run_reader_phase<S>(
    inner: &HubConnectionInner,
    stream: &mut S,
    stop_rx: &mut mpsc::Receiver<()>,
    reconnect_rx: &mut mpsc::Receiver<()>,
    liveness_deadline: Duration,
) -> ConnectedExit
where
    S: Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let mut clock_probe = tokio::time::interval(CLOCK_PROBE_INTERVAL);
    clock_probe.tick().await;
    let deadline = sleep(liveness_deadline);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            biased;
            _ = stop_rx.recv() => return ConnectedExit::Stop,
            _ = reconnect_rx.recv() => {
                info!("forced reconnect requested; dropping current socket");
                return ConnectedExit::SocketClosed(DisconnectCause::Forced);
            }
            // Before the deadline arm so a frame that raced the expiry
            // proves liveness and wins.
            msg = stream.next() => {
                if matches!(msg, Some(Ok(ref m)) if !matches!(m, Message::Close(_))) {
                    inner.health.record_inbound();
                }
                match msg {
                    Some(Ok(msg)) => {
                        // Any inbound frame (data or control) proves liveness,
                        // so re-arm the deadline. Saturate on overflow so a
                        // `Duration::MAX` "disable" override can't panic
                        // `Instant + Duration`.
                        let now = tokio::time::Instant::now();
                        let rearm = now
                            .checked_add(liveness_deadline)
                            .unwrap_or_else(|| now + Duration::from_secs(86400 * 365 * 30));
                        deadline.as_mut().reset(rearm);
                        match msg {
                            Message::Text(text) => {
                                if let Some(pong_text) = route_or_pong(inner, text.as_ref())
                                    && inner.outbound_tx.try_send(pong_text).is_err()
                                {
                                    // App-level pong is JSON text; the reader no longer
                                    // owns the sink, so route it through the writer.
                                    // Best-effort (non-blocking) to keep the reader hot:
                                    // a paused writer (dead socket) or a saturated buffer
                                    // drops the heartbeat. Metered so the residual loss is
                                    // observable/alertable rather than silent.
                                    crate::metrics::heartbeat_pong_dropped();
                                }
                            }
                            // WS control pings get an automatic Pong queued + flushed
                            // by tungstenite on read; nothing to do here.
                            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                            Message::Binary(_) => {
                                warn!("server sent binary frame; ignoring");
                            }
                            Message::Close(frame) => {
                                return exit_for_close_code(frame.map(|f| f.code.into()));
                            }
                        }
                    }
                    Some(Err(e)) => {
                        return ConnectedExit::SocketClosed(classify_stream_end(
                            inner,
                            Some(e.to_string()),
                        ));
                    }
                    None => {
                        return ConnectedExit::SocketClosed(classify_stream_end(inner, None));
                    }
                }
            }
            _ = clock_probe.tick() => inner.health.refresh_clock(),
            _ = &mut deadline => {
                crate::metrics::liveness_deadline_expired();
                warn!(
                    ?liveness_deadline,
                    "no inbound frame within the liveness deadline; declaring the socket dead and reconnecting"
                );
                return ConnectedExit::SocketClosed(DisconnectCause::LivenessDeadline);
            }
        }
    }
}
/// Reconnect once and replay every session binding + tool registration.
async fn reconnect_and_replay(
    inner: &HubConnectionInner,
    url: &Url,
    attempt: u32,
    outage: &OutageInfo,
    backoff_total: Duration,
) -> Result<(SplitSink<WsStream, Message>, SplitStream<WsStream>), ClientError> {
    let fresh_cred = inner.credential.current();
    let ws = open_socket(
        url,
        &fresh_cred,
        inner.kind,
        inner.alpha_test_key.as_deref(),
        inner.allow_insecure_ws,
    )
    .await?;
    let (sink, stream) = ws.split();
    let (mut sink, mut stream, mut ack) = run_handshake(
        sink,
        stream,
        inner.kind,
        inner.server_id.clone(),
        inner.server_description.clone(),
        inner.server_metadata.clone(),
    )
    .await?;
    let sessions = inner.bound_sessions.snapshot_keys();
    if inner.kind == ConnectionKind::Harness {
        for sid in &sessions {
            let req = xai_tool_protocol::JsonRpcRequest {
                jsonrpc: xai_tool_protocol::JsonRpcVersion,
                id: xai_tool_protocol::JsonRpcId::new_uuid_v7(),
                session_id: Some(sid.clone()),
                method: Method::SessionOpen.as_wire_str().to_owned(),
                params: xai_tool_protocol::SessionOpenParams {
                    resume: false,
                    last_seq: None,
                },
            };
            if let Ok(text) = serde_json::to_string(&req) {
                let _ = SinkExt::send(&mut sink, Message::Text(text.into())).await;
                let _ = tokio::time::timeout(Duration::from_secs(5), StreamExt::next(&mut stream))
                    .await;
            }
        }
    }
    let sessions_replayed = sessions.len();
    let silent_gap_ms = outage.last_inbound.elapsed().as_millis() as u64;
    info!(
        attempt,
        sessions_replayed,
        cause = outage.cause.label(),
        close_code = ?outage.cause.close_code(),
        error_detail = ?outage.cause.detail(),
        prev_connection_id = ?outage.prev_connection_id,
        connection_id = %ack.connection_id,
        prev_connection_duration_ms = outage.prev_connection_duration_ms,
        silent_gap_ms,
        detect_ms = outage.detect_ms,
        backoff_total_ms = backoff_total.as_millis() as u64,
        since_last_probe_monotonic_ms = outage.since_last_probe_monotonic_ms,
        since_last_probe_wall_ms = outage.since_last_probe_wall_ms,
        clock_jump_ms = outage.clock_jump_ms,
        "server reconnect succeeded"
    );
    crate::metrics::reconnect_cause(outage.cause.label());
    crate::metrics::reconnect_gap_observe(silent_gap_ms as f64 / 1_000.0);
    *inner.connection_id.lock().await = Some(ack.connection_id.clone());
    *inner.hello_capabilities.write() = std::mem::take(&mut ack.capabilities);
    if let Some(cb) = &inner.on_reconnect {
        cb(ReconnectEvent {
            connection_id: ack.connection_id,
            sessions_replayed,
            attempt,
        });
    }
    Ok((sink, stream))
}
/// Look up the backoff for `attempt` in `schedule`, clamping past the end
/// to the final (cap) slot. Call sites pass a non-empty schedule (resolved
/// via [`resolve_reconnect_backoff`]); the lookup is nonetheless
/// self-contained — an empty slice yields `Duration::ZERO` rather than
/// panicking.
fn backoff_for(attempt: u32, schedule: &[Duration]) -> Duration {
    let idx = (attempt as usize)
        .saturating_sub(1)
        .min(schedule.len().saturating_sub(1));
    schedule.get(idx).copied().unwrap_or_default()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn backoff_for_follows_exponential_schedule() {
        let schedule = default_reconnect_backoff();
        assert_eq!(backoff_for(1, &schedule), Duration::from_millis(100));
        assert_eq!(backoff_for(2, &schedule), Duration::from_millis(200));
        assert_eq!(backoff_for(3, &schedule), Duration::from_millis(500));
        assert_eq!(backoff_for(4, &schedule), Duration::from_millis(1_000));
        assert_eq!(backoff_for(5, &schedule), Duration::from_millis(2_000));
        assert_eq!(backoff_for(6, &schedule), Duration::from_millis(5_000));
        assert_eq!(backoff_for(7, &schedule), Duration::from_millis(10_000));
    }
    #[test]
    fn backoff_for_caps_at_last_slot() {
        let schedule = default_reconnect_backoff();
        let cap = Duration::from_millis(10_000);
        assert_eq!(backoff_for(8, &schedule), cap);
        assert_eq!(backoff_for(50, &schedule), cap);
        assert_eq!(backoff_for(u32::MAX, &schedule), cap);
    }
    #[test]
    fn backoff_for_zero_attempt_uses_first_slot() {
        let schedule = default_reconnect_backoff();
        assert_eq!(backoff_for(0, &schedule), Duration::from_millis(100));
    }
    #[test]
    fn backoff_for_honors_configured_schedule() {
        let schedule = resolve_reconnect_backoff(Some(Arc::from([
            Duration::from_millis(5),
            Duration::from_millis(15),
        ])));
        assert_eq!(backoff_for(1, &schedule), Duration::from_millis(5));
        assert_eq!(backoff_for(2, &schedule), Duration::from_millis(15));
        assert_eq!(backoff_for(3, &schedule), Duration::from_millis(15));
        assert_eq!(backoff_for(99, &schedule), Duration::from_millis(15));
    }
    #[test]
    fn backoff_for_empty_schedule_is_zero_not_panic() {
        assert_eq!(backoff_for(1, &[]), Duration::ZERO);
        assert_eq!(backoff_for(0, &[]), Duration::ZERO);
        assert_eq!(backoff_for(u32::MAX, &[]), Duration::ZERO);
    }
    #[test]
    fn resolve_reconnect_backoff_falls_back_when_unset_or_empty() {
        let from_none = resolve_reconnect_backoff(None);
        let from_empty = resolve_reconnect_backoff(Some(Arc::from([])));
        for schedule in [from_none, from_empty] {
            assert_eq!(backoff_for(1, &schedule), Duration::from_millis(100));
            assert_eq!(backoff_for(7, &schedule), Duration::from_millis(10_000));
            assert_eq!(backoff_for(99, &schedule), Duration::from_millis(10_000));
        }
    }
    /// A zero or unset ping interval must resolve to the default. A zero
    /// period would otherwise reach `tokio::time::interval`, which panics on
    /// `Duration::ZERO`; a positive override is honored verbatim.
    #[test]
    fn resolve_ws_ping_interval_clamps_zero_and_unset_to_default() {
        assert_eq!(resolve_ws_ping_interval(None), DEFAULT_WS_PING_INTERVAL);
        assert_eq!(
            resolve_ws_ping_interval(Some(Duration::ZERO)),
            DEFAULT_WS_PING_INTERVAL
        );
        let custom = Duration::from_secs(7);
        assert_eq!(resolve_ws_ping_interval(Some(custom)), custom);
    }
    /// Resolving a zero ping interval to a non-zero default means
    /// `tokio::time::interval` can be constructed without panicking.
    #[tokio::test]
    async fn resolved_zero_ping_interval_builds_interval_without_panic() {
        let resolved = resolve_ws_ping_interval(Some(Duration::ZERO));
        assert!(!resolved.is_zero());
        let _interval = tokio::time::interval(resolved);
    }
    fn bearer_credential() -> AuthCredential {
        AuthCredential::bearer("test-token")
    }
    #[tokio::test]
    async fn open_socket_refuses_plaintext_ws_to_remote_host() {
        let url = Url::parse("ws://hub.example.com:8080/v1/tools").expect("valid url");
        let credential = bearer_credential();
        match open_socket(&url, &credential, ConnectionKind::Harness, None, false).await {
            Err(ClientError::InsecureScheme { url: rejected }) => {
                assert_eq!(rejected, url);
            }
            other => panic!("expected InsecureScheme; got {other:?}"),
        }
    }
    #[tokio::test]
    async fn open_socket_allows_plaintext_ws_to_loopback() {
        let url = Url::parse("ws://127.0.0.1:1/").expect("valid url");
        let credential = bearer_credential();
        if let Err(ClientError::InsecureScheme { .. }) =
            open_socket(&url, &credential, ConnectionKind::Harness, None, false).await
        {
            panic!("loopback ws:// must not be rejected by the scheme guard")
        }
    }
    #[tokio::test]
    async fn open_socket_allows_wss_to_remote_host() {
        let url = Url::parse("wss://hub.example.com/").expect("valid url");
        let credential = bearer_credential();
        if let Err(ClientError::InsecureScheme { .. }) =
            open_socket(&url, &credential, ConnectionKind::Harness, None, false).await
        {
            panic!("wss:// must not be rejected by the scheme guard")
        }
    }
    #[tokio::test]
    async fn open_socket_allows_plaintext_ws_when_insecure_opt_in() {
        let url = Url::parse("ws://hub.example.com:1/").expect("valid url");
        let credential = bearer_credential();
        if let Err(ClientError::InsecureScheme { .. }) =
            open_socket(&url, &credential, ConnectionKind::Harness, None, true).await
        {
            panic!("allow_insecure_ws must bypass the scheme guard")
        }
    }
    #[tokio::test]
    async fn open_socket_rejects_role_mismatch() {
        let url = Url::parse("ws://127.0.0.1:1/?role=harness").expect("valid url");
        let credential = bearer_credential();
        match open_socket(&url, &credential, ConnectionKind::ToolServer, None, false).await {
            Err(ClientError::InvalidConfig(msg)) => {
                assert!(
                    msg.contains("conflicts with"),
                    "message should mention conflict; got: {msg}"
                );
            }
            other => panic!("expected InvalidConfig; got {other:?}"),
        }
    }
    #[test]
    fn host_is_loopback_recognises_canonical_names() {
        for raw in [
            "ws://127.0.0.1/",
            "ws://[::1]/",
            "ws://localhost/",
            "ws://LOCALHOST/",
        ] {
            let url = Url::parse(raw).expect("valid url");
            assert!(host_is_loopback(&url), "{raw} must be treated as loopback");
        }
        for raw in ["ws://hub.example.com/", "ws://10.0.0.1/", "ws://127.0.0.2/"] {
            let url = Url::parse(raw).expect("valid url");
            assert!(
                !host_is_loopback(&url),
                "{raw} must NOT be treated as loopback",
            );
        }
    }
    #[test]
    fn exit_for_close_code_classifies_terminal_range() {
        assert!(matches!(
            exit_for_close_code(Some(4100)),
            ConnectedExit::TerminalClose(4100)
        ));
        assert!(matches!(
            exit_for_close_code(Some(4199)),
            ConnectedExit::TerminalClose(4199)
        ));
        assert!(matches!(
            exit_for_close_code(Some(4099)),
            ConnectedExit::SocketClosed(DisconnectCause::CloseFrame(Some(4099)))
        ));
        assert!(matches!(
            exit_for_close_code(Some(4200)),
            ConnectedExit::SocketClosed(DisconnectCause::CloseFrame(Some(4200)))
        ));
        assert!(matches!(
            exit_for_close_code(Some(1000)),
            ConnectedExit::SocketClosed(DisconnectCause::CloseFrame(Some(1000)))
        ));
        assert!(matches!(
            exit_for_close_code(None),
            ConnectedExit::SocketClosed(DisconnectCause::CloseFrame(None))
        ));
    }
    #[test]
    fn disconnect_cause_labels_and_fields() {
        assert_eq!(
            DisconnectCause::CloseFrame(Some(1006)).label(),
            "close_frame"
        );
        assert_eq!(
            DisconnectCause::CloseFrame(Some(1006)).close_code(),
            Some(1006)
        );
        assert_eq!(DisconnectCause::Eof.label(), "eof");
        assert_eq!(DisconnectCause::Eof.close_code(), None);
        assert_eq!(DisconnectCause::Eof.detail(), None);
        let read = DisconnectCause::ReadError("reset".to_owned());
        assert_eq!(read.label(), "transport_read_error");
        assert_eq!(read.detail(), Some("reset"));
        let write = DisconnectCause::WriteError("pipe".to_owned());
        assert_eq!(write.label(), "transport_write_error");
        assert_eq!(write.detail(), Some("pipe"));
        assert_eq!(DisconnectCause::Forced.label(), "forced");
    }
    #[test]
    fn conn_health_snapshot_without_clock_skew_reports_zero_jump() {
        let health = ConnHealth::new();
        health.record_inbound();
        health.refresh_clock();
        let snap = health.snapshot();
        assert_eq!(snap.clock_jump_ms, 0);
        assert!(snap.since_last_probe_monotonic_ms < 2_000);
    }
    #[test]
    fn conn_health_snapshot_reports_wall_clock_jump() {
        let health = ConnHealth::new();
        {
            let mut state = health.state.lock();
            state.wall_ref = SystemTime::now() - Duration::from_secs(10);
        }
        let snap = health.snapshot();
        assert!(snap.since_last_probe_wall_ms >= 9_000);
        assert!(snap.since_last_probe_monotonic_ms < 2_000);
        assert!(snap.clock_jump_ms >= 8_000);
        health.reset();
        assert_eq!(health.snapshot().clock_jump_ms, 0);
    }
    #[test]
    fn conn_health_accumulates_jump_across_refreshes() {
        let health = ConnHealth::new();
        {
            let mut state = health.state.lock();
            state.wall_ref = SystemTime::now() - Duration::from_secs(5);
        }
        health.refresh_clock();
        {
            let mut state = health.state.lock();
            state.wall_ref = SystemTime::now() - Duration::from_secs(4);
        }
        let snap = health.snapshot();
        assert!(snap.clock_jump_ms >= 8_000);
    }
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    /// Default ping period for writer tests that don't exercise the
    /// keepalive: long enough that no ping fires during the test.
    const TEST_PING_NEVER: Duration = Duration::from_secs(3_600);
    /// In-memory [`futures::Sink`] for `run_writer` tests. Records the
    /// text payload of every `Message::Text` sent and counts every
    /// `Message::Ping` (keepalive). When the `fail` flag is set, `send`
    /// errors at `poll_ready`, modelling a dead socket.
    #[derive(Clone)]
    struct RecordingSink {
        recorded: Arc<std::sync::Mutex<Vec<String>>>,
        pings: Arc<AtomicUsize>,
        fail: Arc<AtomicBool>,
    }
    impl RecordingSink {
        fn new() -> Self {
            Self {
                recorded: Arc::new(std::sync::Mutex::new(Vec::new())),
                pings: Arc::new(AtomicUsize::new(0)),
                fail: Arc::new(AtomicBool::new(false)),
            }
        }
        fn recorded(&self) -> Arc<std::sync::Mutex<Vec<String>>> {
            self.recorded.clone()
        }
        fn pings(&self) -> Arc<AtomicUsize> {
            self.pings.clone()
        }
        fn fail_flag(&self) -> Arc<AtomicBool> {
            self.fail.clone()
        }
    }
    impl futures::Sink<Message> for RecordingSink {
        type Error = std::io::Error;
        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            if self.fail.load(Ordering::SeqCst) {
                Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "sink dead",
                )))
            } else {
                Poll::Ready(Ok(()))
            }
        }
        fn start_send(self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
            match item {
                Message::Text(text) => {
                    self.recorded
                        .lock()
                        .expect("recorded lock")
                        .push(text.as_str().to_owned());
                }
                Message::Ping(_) => {
                    self.pings.fetch_add(1, Ordering::SeqCst);
                }
                _ => {}
            }
            Ok(())
        }
        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }
    type TestCtl = WriterControl<RecordingSink>;
    fn idle_write_error_slot() -> WriteErrorSlot {
        Arc::new(parking_lot::Mutex::new(None))
    }
    /// Poll `predicate` every 5ms up to ~2s. Keeps the writer-task tests
    /// off arbitrary fixed sleeps for the positive assertions.
    async fn wait_until<F: Fn() -> bool>(predicate: F, label: &str) {
        for _ in 0..400 {
            if predicate() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for: {label}");
    }
    #[tokio::test]
    async fn writer_drains_outbound_while_live() {
        let sink = RecordingSink::new();
        let recorded = sink.recorded();
        let (out_tx, out_rx) = mpsc::channel::<String>(8);
        let (_ctl_tx, ctl_rx) = mpsc::channel::<TestCtl>(2);
        let (stop_tx, stop_rx) = mpsc::channel::<()>(1);
        let writer = tokio::spawn(run_writer(
            sink,
            out_rx,
            ctl_rx,
            stop_rx,
            TEST_PING_NEVER,
            idle_write_error_slot(),
        ));
        out_tx.send("a".to_owned()).await.expect("send a");
        out_tx.send("b".to_owned()).await.expect("send b");
        wait_until(
            || recorded.lock().expect("lock").len() == 2,
            "two frames drained",
        )
        .await;
        assert_eq!(
            *recorded.lock().expect("lock"),
            vec!["a".to_owned(), "b".to_owned()],
            "frames must be written to the live sink in order"
        );
        stop_tx.send(()).await.expect("stop");
        writer.await.expect("writer task joins");
    }
    #[tokio::test]
    async fn writer_honors_custom_ping_interval() {
        let sink = RecordingSink::new();
        let pings = sink.pings();
        let (_out_tx, out_rx) = mpsc::channel::<String>(4);
        let (_ctl_tx, ctl_rx) = mpsc::channel::<TestCtl>(2);
        let (stop_tx, stop_rx) = mpsc::channel::<()>(1);
        let writer = tokio::spawn(run_writer(
            sink,
            out_rx,
            ctl_rx,
            stop_rx,
            Duration::from_millis(20),
            idle_write_error_slot(),
        ));
        wait_until(
            || pings.load(Ordering::SeqCst) >= 3,
            "three keepalive pings at the configured cadence",
        )
        .await;
        stop_tx.send(()).await.expect("stop");
        writer.await.expect("writer task joins");
    }
    #[tokio::test]
    async fn writer_re_arms_custom_ping_interval_after_resume() {
        let dead = RecordingSink::new();
        let (_out_tx, out_rx) = mpsc::channel::<String>(4);
        let (ctl_tx, ctl_rx) = mpsc::channel::<TestCtl>(2);
        let (stop_tx, stop_rx) = mpsc::channel::<()>(1);
        let writer = tokio::spawn(run_writer(
            dead,
            out_rx,
            ctl_rx,
            stop_rx,
            Duration::from_millis(20),
            idle_write_error_slot(),
        ));
        ctl_tx.send(WriterControl::Pause).await.expect("pause");
        let fresh = RecordingSink::new();
        let fresh_pings = fresh.pings();
        ctl_tx
            .send(WriterControl::Resume(fresh))
            .await
            .expect("resume");
        wait_until(
            || fresh_pings.load(Ordering::SeqCst) >= 3,
            "keepalive pings resume on the configured cadence after Resume",
        )
        .await;
        stop_tx.send(()).await.expect("stop");
        writer.await.expect("writer task joins");
    }
    #[tokio::test]
    async fn writer_buffers_during_pause_and_flushes_on_resume() {
        let dead = RecordingSink::new();
        let dead_log = dead.recorded();
        let (out_tx, out_rx) = mpsc::channel::<String>(16);
        let (ctl_tx, ctl_rx) = mpsc::channel::<TestCtl>(2);
        let (stop_tx, stop_rx) = mpsc::channel::<()>(1);
        let writer = tokio::spawn(run_writer(
            dead,
            out_rx,
            ctl_rx,
            stop_rx,
            TEST_PING_NEVER,
            idle_write_error_slot(),
        ));
        ctl_tx.send(WriterControl::Pause).await.expect("pause");
        tokio::time::sleep(Duration::from_millis(20)).await;
        for frame in ["g1", "g2", "g3"] {
            out_tx
                .send(frame.to_owned())
                .await
                .expect("enqueue during gap");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            dead_log.lock().expect("lock").is_empty(),
            "paused writer must not drain onto the dead sink; got {:?}",
            dead_log.lock().expect("lock")
        );
        let fresh = RecordingSink::new();
        let fresh_log = fresh.recorded();
        ctl_tx
            .send(WriterControl::Resume(fresh))
            .await
            .expect("resume");
        wait_until(
            || fresh_log.lock().expect("lock").len() == 3,
            "buffered frames flush after resume",
        )
        .await;
        assert_eq!(
            *fresh_log.lock().expect("lock"),
            vec!["g1".to_owned(), "g2".to_owned(), "g3".to_owned()],
            "all gap frames flush, in order, to the fresh sink"
        );
        assert!(
            dead_log.lock().expect("lock").is_empty(),
            "no frame must ever reach the dead sink"
        );
        stop_tx.send(()).await.expect("stop");
        writer.await.expect("writer task joins");
    }
    #[tokio::test]
    async fn writer_send_error_pauses_until_resume_without_multi_frame_loss() {
        let failing = RecordingSink::new();
        let failing_log = failing.recorded();
        let fail_flag = failing.fail_flag();
        let (out_tx, out_rx) = mpsc::channel::<String>(16);
        let (ctl_tx, ctl_rx) = mpsc::channel::<TestCtl>(2);
        let (stop_tx, stop_rx) = mpsc::channel::<()>(1);
        let write_error = idle_write_error_slot();
        let writer = tokio::spawn(run_writer(
            failing,
            out_rx,
            ctl_rx,
            stop_rx,
            TEST_PING_NEVER,
            write_error.clone(),
        ));
        out_tx.send("ok".to_owned()).await.expect("send ok");
        wait_until(
            || failing_log.lock().expect("lock").len() == 1,
            "first frame drained before failure",
        )
        .await;
        fail_flag.store(true, Ordering::SeqCst);
        out_tx.send("lost".to_owned()).await.expect("enqueue lost");
        out_tx
            .send("kept1".to_owned())
            .await
            .expect("enqueue kept1");
        out_tx
            .send("kept2".to_owned())
            .await
            .expect("enqueue kept2");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            *failing_log.lock().expect("lock"),
            vec!["ok".to_owned()],
            "only the pre-failure frame should have been recorded on the dead sink"
        );
        assert!(
            write_error
                .lock()
                .as_deref()
                .is_some_and(|detail| detail.contains("sink dead")),
            "failed send must record the write-error detail for disconnect classification"
        );
        let fresh = RecordingSink::new();
        let fresh_log = fresh.recorded();
        ctl_tx
            .send(WriterControl::Resume(fresh))
            .await
            .expect("resume");
        wait_until(
            || fresh_log.lock().expect("lock").len() == 2,
            "buffered post-failure frames flush after resume",
        )
        .await;
        assert_eq!(
            *fresh_log.lock().expect("lock"),
            vec!["kept1".to_owned(), "kept2".to_owned()],
            "post-failure frames survive; only the in-flight 'lost' frame is gone"
        );
        stop_tx.send(()).await.expect("stop");
        writer.await.expect("writer task joins");
    }
    #[tokio::test]
    async fn writer_resume_discards_stale_write_error() {
        let sink = RecordingSink::new();
        let (_out_tx, out_rx) = mpsc::channel::<String>(4);
        let (ctl_tx, ctl_rx) = mpsc::channel::<TestCtl>(2);
        let (stop_tx, stop_rx) = mpsc::channel::<()>(1);
        let write_error = idle_write_error_slot();
        let writer = tokio::spawn(run_writer(
            sink,
            out_rx,
            ctl_rx,
            stop_rx,
            TEST_PING_NEVER,
            write_error.clone(),
        ));
        ctl_tx.send(WriterControl::Pause).await.expect("pause");
        *write_error.lock() = Some("frame send failed: stale broken pipe".to_owned());
        ctl_tx
            .send(WriterControl::Resume(RecordingSink::new()))
            .await
            .expect("resume");
        wait_until(
            || write_error.lock().is_none(),
            "Resume must clear a stale write-error left by a late old-sink send",
        )
        .await;
        stop_tx.send(()).await.expect("stop");
        writer.await.expect("writer task joins");
    }
    #[tokio::test]
    async fn writer_exits_on_stop_signal() {
        let sink = RecordingSink::new();
        let (_out_tx, out_rx) = mpsc::channel::<String>(4);
        let (_ctl_tx, ctl_rx) = mpsc::channel::<TestCtl>(2);
        let (stop_tx, stop_rx) = mpsc::channel::<()>(1);
        let writer = tokio::spawn(run_writer(
            sink,
            out_rx,
            ctl_rx,
            stop_rx,
            TEST_PING_NEVER,
            idle_write_error_slot(),
        ));
        stop_tx.send(()).await.expect("stop");
        tokio::time::timeout(Duration::from_secs(2), writer)
            .await
            .expect("writer must exit on the stop signal")
            .expect("writer task joins");
    }
    #[tokio::test]
    async fn writer_exits_when_outbound_channel_closes() {
        let sink = RecordingSink::new();
        let (out_tx, out_rx) = mpsc::channel::<String>(4);
        let (_ctl_tx, ctl_rx) = mpsc::channel::<TestCtl>(2);
        let (_stop_tx, stop_rx) = mpsc::channel::<()>(1);
        let writer = tokio::spawn(run_writer(
            sink,
            out_rx,
            ctl_rx,
            stop_rx,
            TEST_PING_NEVER,
            idle_write_error_slot(),
        ));
        drop(out_tx);
        tokio::time::timeout(Duration::from_secs(2), writer)
            .await
            .expect("writer must exit when outbound closes")
            .expect("writer task joins");
    }
    #[tokio::test]
    async fn writer_exits_when_control_channel_closes() {
        let sink = RecordingSink::new();
        let (_out_tx, out_rx) = mpsc::channel::<String>(4);
        let (ctl_tx, ctl_rx) = mpsc::channel::<TestCtl>(2);
        let (_stop_tx, stop_rx) = mpsc::channel::<()>(1);
        let writer = tokio::spawn(run_writer(
            sink,
            out_rx,
            ctl_rx,
            stop_rx,
            TEST_PING_NEVER,
            idle_write_error_slot(),
        ));
        drop(ctl_tx);
        tokio::time::timeout(Duration::from_secs(2), writer)
            .await
            .expect("writer must exit when the control channel closes")
            .expect("writer task joins");
    }
    /// Socket-less `HubConnection` for tests: observe the sent frame and
    /// resolve the response waiter without a live server or actor task.
    fn test_connection() -> (Arc<HubConnection>, Arc<Demux>, mpsc::Receiver<String>) {
        let (outbound_tx, outbound_rx) = mpsc::channel::<String>(8);
        let demux = Arc::new(Demux::with_outbound(outbound_tx.clone()));
        let credential: Arc<dyn AuthProvider> = Arc::new(AuthCredential::bearer("test-token"));
        let (stop_tx, _stop_rx) = mpsc::channel::<()>(1);
        let (reconnect_tx, _reconnect_rx) = mpsc::channel::<()>(1);
        let inner = Arc::new(HubConnectionInner {
            key: ConnKey {
                url: "ws://test/v1/tools".to_owned(),
                principal: credential.principal_key(),
            },
            kind: ConnectionKind::ToolServer,
            credential,
            on_reconnect: None,
            on_disconnect: None,
            server_id: None,
            server_description: None,
            server_metadata: None,
            alpha_test_key: None,
            allow_insecure_ws: false,
            on_fatal: None,
            reconnect_backoff: resolve_reconnect_backoff(None),
            outbound_tx,
            demux: demux.clone(),
            bound_sessions: Arc::new(RefCountedSet::new()),
            connection_id: Arc::new(Mutex::new(None)),
            hello_capabilities: parking_lot::RwLock::new(Vec::new()),
            next_request_id: std::sync::atomic::AtomicU64::new(1),
            shutdown: CancellationToken::new(),
            stop_tx,
            reconnect_tx,
            early_notif_rx: parking_lot::Mutex::new(Some(demux.subscribe_notifications())),
            health: ConnHealth::new(),
            writer_error: Arc::new(parking_lot::Mutex::new(None)),
        });
        (Arc::new(HubConnection { inner }), demux, outbound_rx)
    }
    #[test]
    fn classify_stream_end_prefers_recorded_write_error() {
        let (conn, _demux, _outbound_rx) = test_connection();
        let inner = conn.inner.as_ref();
        assert!(matches!(
            classify_stream_end(inner, None),
            DisconnectCause::Eof
        ));
        assert!(matches!(
            classify_stream_end(inner, Some("reset by peer".to_owned())),
            DisconnectCause::ReadError(detail) if detail == "reset by peer"
        ));
        *inner.writer_error.lock() = Some("ping send failed: broken pipe".to_owned());
        assert!(matches!(
            classify_stream_end(inner, None),
            DisconnectCause::WriteError(detail) if detail == "ping send failed: broken pipe"
        ));
        assert!(
            inner.writer_error.lock().is_none(),
            "classification must consume the recorded write error"
        );
        *inner.writer_error.lock() = Some("frame send failed: broken pipe".to_owned());
        assert!(matches!(
            classify_stream_end(inner, Some("reset".to_owned())),
            DisconnectCause::WriteError(_)
        ));
    }
    #[test]
    fn supports_is_unknown_until_capabilities_advertised() {
        let (conn, _demux, _outbound_rx) = test_connection();
        assert_eq!(conn.supports("session_attach_server"), None);
        *conn.inner.hello_capabilities.write() = vec!["session_attach_server".to_owned()];
        assert_eq!(conn.supports("session_attach_server"), Some(true));
        assert_eq!(conn.supports("some_other_method"), Some(false));
    }
    #[tokio::test]
    async fn call_request_with_timeout_round_trips_via_demux() {
        let (conn, demux, mut outbound_rx) = test_connection();
        let session = SessionId::new("rt_session").expect("valid");
        let request_id = conn.try_alloc_request_id().expect("request id");
        let id_str = request_id.to_string();
        let req = JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: JsonRpcId::from_request_id(&request_id),
            session_id: Some(session.clone()),
            method: Method::Hook.as_wire_str().to_owned(),
            params: serde_json::json!({ "k": "v" }),
        };
        let call = tokio::spawn(async move {
            conn.call_request_with_timeout(request_id, &req, Duration::from_secs(5))
                .await
        });
        let sent = tokio::time::timeout(Duration::from_secs(1), outbound_rx.recv())
            .await
            .expect("frame sent before deadline")
            .expect("outbound frame present");
        let sent_value: Value = serde_json::from_str(&sent).expect("sent frame is valid json");
        assert_eq!(sent_value["id"].as_str(), Some(id_str.as_str()));
        assert_eq!(
            sent_value["method"].as_str(),
            Some(Method::Hook.as_wire_str())
        );
        let outcome = demux.route(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id_str,
            "session_id": session.as_str(),
            "result": { "ok": true },
        }));
        assert_eq!(outcome, crate::demux::RouteOutcome::Response);
        let resp = call
            .await
            .expect("call task joins")
            .expect("call resolves with a response");
        let ResponseOutcome::Result(value) = resp.outcome else {
            panic!("expected a result outcome");
        };
        assert_eq!(value, serde_json::json!({ "ok": true }));
    }
    #[tokio::test]
    async fn call_request_reclaims_waiter_on_send_failure() {
        let (conn, demux, outbound_rx) = test_connection();
        drop(outbound_rx);
        let request_id = conn.try_alloc_request_id().expect("request id");
        let probe_id = request_id.clone();
        let req = JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: JsonRpcId::from_request_id(&request_id),
            session_id: None,
            method: Method::Hook.as_wire_str().to_owned(),
            params: serde_json::json!({}),
        };
        let result = conn.call_request(request_id, &req).await;
        assert!(matches!(result, Err(ClientError::NetworkError(_))));
        assert!(
            demux.take_response_waiter(&probe_id).is_none(),
            "the failed-send waiter is reclaimed so it cannot leak"
        );
    }
    #[tokio::test]
    async fn serve_send_failure_fails_fast_without_retry() {
        let (conn, demux, outbound_rx) = test_connection();
        drop(outbound_rx);
        let session = SessionId::new("serve_session").expect("valid");
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            conn.serve(session, xai_tool_protocol::ServeParams { tools: vec![] }),
        )
        .await
        .expect("serve must fail bounded, not park");
        assert!(matches!(result, Err(ClientError::NetworkError(_))));
        let request_id = xai_tool_protocol::RequestId::new("c1").expect("valid");
        assert!(
            demux.take_response_waiter(&request_id).is_none(),
            "the failed attempt must not leak a waiter"
        );
        assert_eq!(
            conn.try_alloc_request_id().expect("request id").to_string(),
            "c2",
            "a non-timeout failure must consume a single attempt, not retry"
        );
    }
    #[tokio::test(start_paused = true)]
    async fn serve_times_out_bounded_and_reclaims_every_attempt_waiter() {
        let (conn, demux, mut outbound_rx) = test_connection();
        let session = SessionId::new("serve_timeout").expect("valid");
        let result = conn
            .serve(session, xai_tool_protocol::ServeParams { tools: vec![] })
            .await;
        assert!(matches!(result, Err(ClientError::NetworkError(_))));
        for id in ["c1", "c2", "c3"] {
            let sent = outbound_rx.try_recv().expect("attempt frame sent");
            let value: Value = serde_json::from_str(&sent).expect("valid json");
            assert_eq!(value["id"].as_str(), Some(id));
            let request_id = xai_tool_protocol::RequestId::new(id).expect("valid");
            assert!(
                demux.take_response_waiter(&request_id).is_none(),
                "attempt {id} must not leak a waiter"
            );
        }
        assert!(
            outbound_rx.try_recv().is_err(),
            "exactly SERVE_MAX_ATTEMPTS frames are sent"
        );
    }
    #[tokio::test]
    async fn call_request_reclaims_waiter_on_caller_cancellation() {
        let (conn, demux, mut outbound_rx) = test_connection();
        let request_id = conn.try_alloc_request_id().expect("request id");
        let probe_id = request_id.clone();
        let conn_for_call = conn.clone();
        let call = tokio::spawn(async move {
            let req = JsonRpcRequest {
                jsonrpc: JsonRpcVersion,
                id: JsonRpcId::from_request_id(&request_id),
                session_id: None,
                method: Method::Hook.as_wire_str().to_owned(),
                params: serde_json::json!({}),
            };
            conn_for_call.call_request(request_id, &req).await
        });
        tokio::time::timeout(Duration::from_secs(1), outbound_rx.recv())
            .await
            .expect("frame sent")
            .expect("outbound frame present");
        call.abort();
        let _ = call.await;
        assert!(
            demux.take_response_waiter(&probe_id).is_none(),
            "the cancelled caller's waiter is reclaimed so it cannot leak"
        );
    }
    #[tokio::test]
    async fn reader_phase_exits_socket_closed_on_forced_reconnect_signal() {
        let (conn, _demux, _outbound_rx) = test_connection();
        let (reconnect_tx, mut reconnect_rx) = mpsc::channel::<()>(1);
        let (_stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        reconnect_tx.try_send(()).expect("queue forced reconnect");
        let mut stream =
            futures::stream::pending::<Result<Message, tokio_tungstenite::tungstenite::Error>>();
        let exit = tokio::time::timeout(
            Duration::from_secs(1),
            run_reader_phase(
                conn.inner.as_ref(),
                &mut stream,
                &mut stop_rx,
                &mut reconnect_rx,
                Duration::from_secs(75),
            ),
        )
        .await
        .expect("forced reconnect must break the reader phase");
        assert!(
            matches!(exit, ConnectedExit::SocketClosed(DisconnectCause::Forced)),
            "a forced reconnect exits as SocketClosed (drives the reconnect path)"
        );
    }
    #[tokio::test]
    async fn stop_signal_outranks_forced_reconnect() {
        let (conn, _demux, _outbound_rx) = test_connection();
        let (reconnect_tx, mut reconnect_rx) = mpsc::channel::<()>(1);
        let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        reconnect_tx.try_send(()).expect("queue forced reconnect");
        stop_tx.try_send(()).expect("queue stop");
        let mut stream =
            futures::stream::pending::<Result<Message, tokio_tungstenite::tungstenite::Error>>();
        let exit = tokio::time::timeout(
            Duration::from_secs(1),
            run_reader_phase(
                conn.inner.as_ref(),
                &mut stream,
                &mut stop_rx,
                &mut reconnect_rx,
                Duration::from_secs(75),
            ),
        )
        .await
        .expect("stop must break the reader phase");
        assert!(matches!(exit, ConnectedExit::Stop));
    }
    #[tokio::test]
    async fn drain_reconnect_signals_clears_stale_signal_only() {
        let (reconnect_tx, mut reconnect_rx) = mpsc::channel::<()>(1);
        reconnect_tx.try_send(()).expect("queue stale signal");
        drain_reconnect_signals(&mut reconnect_rx);
        assert!(
            reconnect_rx.try_recv().is_err(),
            "a stale pre-reconnect signal is consumed by the drain"
        );
        reconnect_tx.try_send(()).expect("queue fresh signal");
        assert!(
            reconnect_rx.try_recv().is_ok(),
            "the drain must not disable the channel for future signals"
        );
    }
    #[tokio::test]
    async fn early_subscribed_receiver_buffers_pre_run_connection_notifications() {
        let (conn, demux, _outbound_rx) = test_connection();
        let outcome = demux.route(serde_json::json!({
            "jsonrpc": "2.0",
            "id": "b1",
            "method": "session.bind",
            "params": { "session_id": "s1" },
        }));
        assert_eq!(outcome, crate::demux::RouteOutcome::Notification);
        let mut rx = conn
            .take_early_notifications()
            .expect("receiver retained until taken");
        let frame = rx.try_recv().expect("pre-run frame buffered");
        assert_eq!(frame["method"], "session.bind");
        assert!(
            conn.take_early_notifications().is_none(),
            "the early receiver is handed off exactly once"
        );
    }
    #[tokio::test]
    async fn call_request_with_timeout_reclaims_waiter_on_deadline() {
        let (conn, demux, mut outbound_rx) = test_connection();
        let session = SessionId::new("to_session").expect("valid");
        let request_id = conn.try_alloc_request_id().expect("request id");
        let probe_id = request_id.clone();
        let req = JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: JsonRpcId::from_request_id(&request_id),
            session_id: Some(session),
            method: Method::Hook.as_wire_str().to_owned(),
            params: serde_json::json!({}),
        };
        let result = conn
            .call_request_with_timeout(request_id, &req, Duration::from_millis(50))
            .await;
        assert!(matches!(result, Err(ClientError::NetworkError(_))));
        assert!(
            outbound_rx.try_recv().is_ok(),
            "the request frame is sent before the deadline fires"
        );
        assert!(
            demux.take_response_waiter(&probe_id).is_none(),
            "the timed-out waiter is reclaimed so it cannot leak"
        );
    }
    /// Regression: a *forced* reconnect abandons a still-healthy socket. If
    /// the first reconnect attempt then fails, the actor must keep retrying
    /// off the abandoned stream — falling back into the reader phase would
    /// park in `stream.next()` on the live old connection forever (the
    /// reconnect signal was already consumed), stalling the retry loop.
    ///
    /// Mock: conn #0 (initial) completes the handshake and stays healthy;
    /// conn #1 (first reconnect) is dropped before the ack (transport
    /// failure); conn #2 must then be attempted and complete. With the bug,
    /// upgrade #2 never happens and the test times out.
    #[tokio::test]
    async fn forced_reconnect_retries_past_failed_attempt_without_repolling_old_stream() {
        use futures::{SinkExt as _, StreamExt as _};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock hub");
        let addr = listener.local_addr().expect("mock addr");
        let upgrades = Arc::new(AtomicUsize::new(0));
        let upgrades_srv = upgrades.clone();
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let n = upgrades_srv.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else {
                        return;
                    };
                    if n == 1 {
                        return;
                    }
                    let _ = ws.next().await;
                    let ack = serde_json::json!({
                        "connection_id": format!("mock-conn-{n}"),
                        "user_id": "test",
                        "computer_hub_version": "test",
                        "supported_protocol_versions": ["1.0.0"],
                    });
                    if ws
                        .send(tokio_tungstenite::tungstenite::Message::Text(
                            ack.to_string().into(),
                        ))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    while let Some(msg) = ws.next().await {
                        if msg.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        let credential: Arc<dyn AuthProvider> = Arc::new(AuthCredential::bearer("test-token"));
        let conn = HubConnection::connect(ConnectionConfig {
            url: url::Url::parse(&format!("ws://{addr}/v1/tools")).expect("mock url"),
            credential,
            kind: ConnectionKind::ToolServer,
            on_reconnect: None,
            on_disconnect: None,
            on_connect: None,
            server_id: None,
            server_description: None,
            server_metadata: None,
            outbound_buffer: None,
            tuning: ConnectionTuning {
                reconnect_backoff: Some(Arc::from([Duration::from_millis(10)])),
                ..Default::default()
            },
            alpha_test_key: None,
            allow_insecure_ws: false,
            on_fatal: None,
        })
        .await
        .expect("initial connect");
        conn.force_reconnect();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while upgrades.load(Ordering::SeqCst) < 3 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "retry stalled after a failed forced-reconnect attempt: \
                 {} upgrades observed (expected 3: initial + failed + successful)",
                upgrades.load(Ordering::SeqCst)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        conn.request_shutdown();
        conn.await_shutdown().await;
    }
    #[tokio::test]
    async fn call_request_serialization_failure_registers_no_waiter() {
        struct FailingParams;
        impl serde::Serialize for FailingParams {
            fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("intentionally unserializable"))
            }
        }
        let (conn, demux, _outbound_rx) = test_connection();
        let session = SessionId::new("serde_fail_session").expect("valid");
        let request_id = conn.try_alloc_request_id().expect("request id");
        let probe_id = request_id.clone();
        let req = JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: JsonRpcId::from_request_id(&request_id),
            session_id: Some(session),
            method: Method::Hook.as_wire_str().to_owned(),
            params: FailingParams,
        };
        let result = conn.call_request(request_id, &req).await;
        assert!(result.is_err(), "serialization failure must surface");
        assert!(
            demux.take_response_waiter(&probe_id).is_none(),
            "no waiter may be registered when serialization fails"
        );
    }
    type WsError = tokio_tungstenite::tungstenite::Error;
    type InboundTx = futures::channel::mpsc::UnboundedSender<Result<Message, WsError>>;
    type InboundRx = futures::channel::mpsc::UnboundedReceiver<Result<Message, WsError>>;
    /// In-memory inbound frame source for `run_reader_phase` tests
    /// (mirrors `RecordingSink` for the writer half).
    fn test_inbound() -> (InboundTx, InboundRx) {
        futures::channel::mpsc::unbounded()
    }
    /// A zero or unset liveness deadline resolves to 2.5× the effective
    /// ping cadence; a positive override is honored verbatim. Mirrors the
    /// `resolve_ws_ping_interval` clamp semantics.
    #[test]
    fn resolve_ws_liveness_deadline_clamps_zero_and_unset_to_default() {
        let ping = Duration::from_secs(30);
        assert_eq!(
            resolve_ws_liveness_deadline(None, ping),
            Duration::from_secs(75)
        );
        assert_eq!(
            resolve_ws_liveness_deadline(Some(Duration::ZERO), ping),
            Duration::from_secs(75)
        );
        let custom = Duration::from_secs(120);
        assert_eq!(resolve_ws_liveness_deadline(Some(custom), ping), custom);
    }
    /// The per-attempt reconnect budget tracks the liveness deadline above
    /// the floor and is clamped to the floor below it, so a small liveness
    /// override can never starve connection establishment.
    #[test]
    fn reconnect_attempt_budget_floors_small_deadlines() {
        assert_eq!(
            reconnect_attempt_budget(Duration::from_millis(2_500)),
            RECONNECT_ATTEMPT_MIN_BUDGET
        );
        assert_eq!(
            reconnect_attempt_budget(RECONNECT_ATTEMPT_MIN_BUDGET),
            RECONNECT_ATTEMPT_MIN_BUDGET
        );
        let large = Duration::from_secs(300);
        assert_eq!(reconnect_attempt_budget(large), large);
    }
    #[test]
    fn resolve_ws_liveness_deadline_scales_with_ping_override() {
        assert_eq!(
            resolve_ws_liveness_deadline(None, Duration::from_secs(10)),
            Duration::from_secs(25)
        );
    }
    #[tokio::test(start_paused = true)]
    async fn reader_deadline_kills_silently_dead_connection() {
        let (conn, _demux, _outbound_rx) = test_connection();
        let (inbound_tx, mut inbound_rx) = test_inbound();
        let (_stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        let (_reconnect_tx, mut reconnect_rx) = mpsc::channel::<()>(1);
        let liveness = Duration::from_secs(75);
        let start = tokio::time::Instant::now();
        let exit = run_reader_phase(
            &conn.inner,
            &mut inbound_rx,
            &mut stop_rx,
            &mut reconnect_rx,
            liveness,
        )
        .await;
        assert!(matches!(
            exit,
            ConnectedExit::SocketClosed(DisconnectCause::LivenessDeadline)
        ));
        assert_eq!(
            start.elapsed(),
            liveness,
            "expiry exactly one liveness window after (re)entry"
        );
        drop(inbound_tx);
    }
    #[tokio::test(start_paused = true)]
    async fn reader_deadline_rearms_on_any_inbound_frame() {
        let (conn, _demux, _outbound_rx) = test_connection();
        let (inbound_tx, mut inbound_rx) = test_inbound();
        let (_stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        let (_reconnect_tx, mut reconnect_rx) = mpsc::channel::<()>(1);
        let liveness = Duration::from_secs(75);
        let phase = run_reader_phase(
            &conn.inner,
            &mut inbound_rx,
            &mut stop_rx,
            &mut reconnect_rx,
            liveness,
        );
        tokio::pin!(phase);
        let frames = [
            Message::Pong(Vec::new().into()),
            Message::Ping(Vec::new().into()),
            Message::Text(r#"{"jsonrpc":"2.0","method":"noop","params":{}}"#.into()),
            Message::Pong(Vec::new().into()),
        ];
        for frame in frames {
            tokio::time::advance(liveness * 3 / 4).await;
            inbound_tx.unbounded_send(Ok(frame)).expect("send frame");
            assert!(
                futures::poll!(phase.as_mut()).is_pending(),
                "phase must stay live while frames keep arriving"
            );
        }
        tokio::time::advance(liveness - Duration::from_millis(1)).await;
        assert!(
            futures::poll!(phase.as_mut()).is_pending(),
            "still inside the window re-armed by the last frame"
        );
        tokio::time::advance(Duration::from_millis(1)).await;
        match futures::poll!(phase.as_mut()) {
            std::task::Poll::Ready(exit) => {
                assert!(matches!(
                    exit,
                    ConnectedExit::SocketClosed(DisconnectCause::LivenessDeadline)
                ));
            }
            std::task::Poll::Pending => {
                panic!("deadline must fire one window after the last frame")
            }
        }
    }
    #[tokio::test(start_paused = true)]
    async fn reader_deadline_huge_override_saturates_instead_of_panicking() {
        let (conn, _demux, _outbound_rx) = test_connection();
        let (inbound_tx, mut inbound_rx) = test_inbound();
        let (_stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        let (_reconnect_tx, mut reconnect_rx) = mpsc::channel::<()>(1);
        let phase = run_reader_phase(
            &conn.inner,
            &mut inbound_rx,
            &mut stop_rx,
            &mut reconnect_rx,
            Duration::MAX,
        );
        tokio::pin!(phase);
        inbound_tx
            .unbounded_send(Ok(Message::Pong(Vec::new().into())))
            .expect("send frame");
        assert!(
            futures::poll!(phase.as_mut()).is_pending(),
            "saturating re-arm must neither panic nor fire"
        );
    }
    /// Sink for writer↔reader composition tests: echoes every keepalive
    /// `Ping` back as a `Pong` on the reader's inbound channel, emulating a
    /// healthy server whose only traffic is the keepalive exchange.
    struct PongEchoSink {
        inbound: InboundTx,
    }
    impl futures::Sink<Message> for PongEchoSink {
        type Error = std::io::Error;
        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn start_send(self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
            if let Message::Ping(payload) = item {
                let _ = self.inbound.unbounded_send(Ok(Message::Pong(payload)));
            }
            Ok(())
        }
        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }
    #[tokio::test(start_paused = true)]
    async fn default_ping_pong_composition_keeps_idle_connection_alive() {
        let ping = resolve_ws_ping_interval(None);
        let deadline = resolve_ws_liveness_deadline(None, ping);
        let (conn, _demux, _outbound_rx) = test_connection();
        let (inbound_tx, mut inbound_rx) = test_inbound();
        let (_out_tx, out_rx) = mpsc::channel::<String>(4);
        let (ctl_tx, ctl_rx) = mpsc::channel::<WriterControl<PongEchoSink>>(2);
        let (writer_stop_tx, writer_stop_rx) = mpsc::channel::<()>(1);
        let writer = tokio::spawn(run_writer(
            PongEchoSink {
                inbound: inbound_tx.clone(),
            },
            out_rx,
            ctl_rx,
            writer_stop_rx,
            ping,
            idle_write_error_slot(),
        ));
        let (_stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        let (_reconnect_tx, mut reconnect_rx) = mpsc::channel::<()>(1);
        {
            let phase = run_reader_phase(
                &conn.inner,
                &mut inbound_rx,
                &mut stop_rx,
                &mut reconnect_rx,
                deadline,
            );
            tokio::pin!(phase);
            tokio::select! {
                _ = phase.as_mut() => panic!("idle-but-healthy connection tripped the deadline"),
                _ = tokio::time::sleep(deadline * 4) => {}
            }
        }
        ctl_tx.send(WriterControl::Pause).await.expect("pause");
        let (fresh_tx, mut fresh_rx) = test_inbound();
        ctl_tx
            .send(WriterControl::Resume(PongEchoSink { inbound: fresh_tx }))
            .await
            .expect("resume");
        {
            let phase = run_reader_phase(
                &conn.inner,
                &mut fresh_rx,
                &mut stop_rx,
                &mut reconnect_rx,
                deadline,
            );
            tokio::pin!(phase);
            tokio::select! {
                _ = phase.as_mut() => {
                    panic!("idle connection tripped the deadline after Pause→Resume")
                }
                _ = tokio::time::sleep(deadline * 4) => {}
            }
        }
        writer_stop_tx.send(()).await.expect("stop");
        writer.await.expect("writer task joins");
    }
    #[tokio::test]
    async fn reader_phase_close_frame_classification_unchanged() {
        use tokio_tungstenite::tungstenite::protocol::CloseFrame;
        use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
        let (conn, _demux, _outbound_rx) = test_connection();
        let (inbound_tx, mut inbound_rx) = test_inbound();
        let (_stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        let (_reconnect_tx, mut reconnect_rx) = mpsc::channel::<()>(1);
        inbound_tx
            .unbounded_send(Ok(Message::Close(Some(CloseFrame {
                code: CloseCode::from(4100),
                reason: "evicted".into(),
            }))))
            .expect("send close");
        let exit = run_reader_phase(
            &conn.inner,
            &mut inbound_rx,
            &mut stop_rx,
            &mut reconnect_rx,
            Duration::from_secs(75),
        )
        .await;
        assert!(matches!(exit, ConnectedExit::TerminalClose(4100)));
    }
}
