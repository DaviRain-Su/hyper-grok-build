use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
/// The binary version of the currently running leader process.
///
/// Compared against each registering client's `ClientCapabilities::client_version`
/// to detect mismatches early and surface a structured ACP notification.
/// In development builds where `VERSION_WITH_COMMIT` is not set, this is
/// `"unknown"` and version-mismatch detection is disabled (no notification sent).
const LEADER_VERSION: &str = match option_env!("VERSION_WITH_COMMIT") {
    Some(v) => v,
    None => "unknown",
};
use super::protocol::{
    ClientCapabilities, ClientId, ClientMessage, ClientMode, ControlCommand, ControlPayload,
    LEADER_PROTOCOL_VERSION, LeaderCapabilities, ProtocolError, ServerMessage, read_message,
    write_message,
};
use super::transport::{LeaderListener, LeaderStream};
use crate::agent::activity::AgentActivity;
use crate::auth::AuthManager;
use crate::cpu_profile::{
    ControlError, ControlErrorCode, CpuProfileManager, CpuProfileStartOptions, CpuProfileStatus,
    ShutdownStopDisposition,
};
use agent_client_protocol::AGENT_METHOD_NAMES;
use kanal::{AsyncReceiver, AsyncSender};
use parking_lot::Mutex;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};
use xai_computer_hub_sdk::{AuthCredential, AuthIdentity, AuthProvider};
use xai_grok_workspace::WorkspaceHandle;
const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(30);
/// Separator for namespacing request IDs. Using pipe character which is:
/// - Valid in JSON strings (no escaping needed)
/// - Unlikely to appear in typical JSON-RPC IDs (usually numbers or UUIDs)
const ID_NAMESPACE_SEP: char = '|';
/// Cap on live notifications buffered per in-flight `session/load` (see
/// `load_live_buffer`). A normal load resolves in well under a second, so the
/// buffer is tiny; this bound just prevents unbounded growth if a load stalls.
/// On overflow we stop buffering and forward live normally (correctness of the
/// transcript is preserved by the client's eventId dedup; only the ordering
/// nicety is lost in this degenerate case).
const MAX_BUFFERED_LIVE_PER_LOAD: usize = 4096;
enum ServerEvent {
    Disconnected(ClientId),
    Registered(ClientId, ClientMode, ClientCapabilities, String),
    Message(ClientId, ClientMessage),
}
enum LeaderServerPoll {
    Cancelled,
    Accept(std::io::Result<LeaderStream>),
    Event(ServerEvent),
    Response(String),
}
/// A live notification buffered during an in-flight `session/load`: the
/// shared payload plus its `event_seq` (computed at buffer time, when the
/// message is already parsed, so the post-load flush never re-parses).
type BufferedLive = (Arc<str>, Option<u64>);
/// Message queued to a client handler task.
///
/// ACP payloads are by far the hot path (every chunk of every session fans out
/// to every subscriber), so they ride as a shared `Arc<str>`: the routing loop
/// pays one refcount bump per recipient instead of a full `String` clone, and
/// the live-load buffer / interaction cache share the same allocation. The
/// handler serializes the wire envelope via [`ServerMessageRef`] without ever
/// materializing an owned `ServerMessage::Acp`.
#[derive(Debug, Clone)]
enum ClientOutbound {
    /// An ACP payload, shared (refcounted) across fan-out targets.
    Acp(Arc<str>),
    /// Everything else (registration, control results, ping, shutdown, errors).
    Message(ServerMessage),
}
impl From<ServerMessage> for ClientOutbound {
    fn from(msg: ServerMessage) -> Self {
        Self::Message(msg)
    }
}
/// Serialize-only mirror of [`ServerMessage`]'s `Acp` variant that borrows the
/// payload, so the per-client writer can frame a shared `Arc<str>` without
/// copying it into an owned `ServerMessage`. Must stay wire-identical to
/// `ServerMessage::Acp` — asserted by the
/// `server_message_ref_is_wire_identical` test.
#[derive(serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessageRef<'a> {
    Acp { payload: &'a str },
}
/// Write one [`ClientOutbound`] to a client connection.
async fn write_outbound<W>(writer: &mut W, msg: &ClientOutbound) -> Result<(), ProtocolError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    match msg {
        ClientOutbound::Acp(payload) => {
            write_message(writer, &ServerMessageRef::Acp { payload }).await
        }
        ClientOutbound::Message(m) => write_message(writer, m).await,
    }
}
struct ClientState {
    tx: AsyncSender<ClientOutbound>,
    mode: ClientMode,
    capabilities: ClientCapabilities,
    /// The client type string from IPC registration (e.g., "grok-tui", "grok-code-extension").
    /// Injected into `initialize` requests as `clientIdentifier` so the agent knows the real
    /// client type even when multiple clients share one leader process.
    client_type: String,
    /// Set to `true` once the client's `initialize` request has been seen and had
    /// `clientIdentifier` injected. Until `initialize` is observed (regardless of how many
    /// earlier messages arrived), each ACP message is checked so we never miss a late
    /// `initialize`. After it is seen once, we skip the per-message parse as an optimisation.
    initialize_seen: bool,
    /// Patch the next response's `modelState.currentModelId` to match `default_model`.
    /// Set on outbound `initialize`, cleared after patching the response.
    patch_initialize_model: bool,
    /// Whether this client has completed IPC registration. Used to keep `client_count`
    /// accurate — only registered clients are counted, so pre-registration connections
    /// (which may time out) don't inflate the count and block auto-updates.
    registered: bool,
}
#[derive(Debug, Clone)]
pub struct LeaderServerMetadata {
    pub pid: u32,
    pub socket_path: PathBuf,
    pub lock_path: PathBuf,
    pub ws_url_suffix: String,
    pub leader_binary_version: String,
}
#[derive(Debug, Clone)]
pub struct LeaderServerControlState {
    pub metadata: LeaderServerMetadata,
    pub cpu_profile: Arc<Mutex<CpuProfileManager>>,
    pub workspace: Arc<WorkspaceControl>,
}
impl LeaderServerControlState {
    pub fn new(metadata: LeaderServerMetadata) -> Self {
        Self {
            metadata,
            cpu_profile: Arc::new(Mutex::new(CpuProfileManager::new())),
            workspace: Arc::new(WorkspaceControl::new(None)),
        }
    }
    pub fn with_default_hub_url(mut self, default_hub_url: Option<String>) -> Self {
        self.workspace = Arc::new(WorkspaceControl::new(default_hub_url));
        self
    }
    fn leader_capabilities(&self) -> LeaderCapabilities {
        let manager = self.cpu_profile.lock();
        LeaderCapabilities {
            control_v1: true,
            runtime_cpu_profile: manager.runtime_cpu_profile(),
            profile_formats: manager.profile_formats().to_vec(),
            workspace_exposure: true,
            relaunch_v1: true,
        }
    }
}
pub struct WorkspaceControl {
    default_hub_url: Option<String>,
    /// Hub credential, wired to the leader's `AuthManager` once auth is ready.
    /// A `watch` so a starting leader (socket up, auth pending) can be awaited
    /// instead of failing the command.
    auth: tokio::sync::watch::Sender<Option<Arc<dyn AuthProvider>>>,
    /// Serializes mutating commands (start/pause/resume/stop) so their long
    /// awaits (drain, reconnect) never interleave.
    lock: tokio::sync::Mutex<()>,
    /// Current exposure, published for lock-free reads so `status` never
    /// blocks behind an in-flight drain/reconnect.
    exposure: arc_swap::ArcSwapOption<WorkspaceExposure>,
}
impl WorkspaceControl {
    fn new(default_hub_url: Option<String>) -> Self {
        Self {
            default_hub_url,
            auth: tokio::sync::watch::channel(None).0,
            lock: tokio::sync::Mutex::new(()),
            exposure: arc_swap::ArcSwapOption::empty(),
        }
    }
    /// Wire the hub credential to the leader's shared `AuthManager` (sole
    /// owner of refresh + persistence).
    pub fn set_auth_manager(&self, auth_manager: Arc<AuthManager>) {
        self.auth
            .send_replace(Some(Arc::new(LeaderAuthProvider { auth_manager })));
    }
}
impl std::fmt::Debug for WorkspaceControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceControl")
            .field("default_hub_url", &self.default_hub_url)
            .finish_non_exhaustive()
    }
}
/// Hub [`AuthProvider`] backed by the leader's `AuthManager`: returns the
/// current token at each connect/reconnect; never writes auth.json.
struct LeaderAuthProvider {
    auth_manager: Arc<AuthManager>,
}
impl std::fmt::Debug for LeaderAuthProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaderAuthProvider").finish_non_exhaustive()
    }
}
impl AuthProvider for LeaderAuthProvider {
    fn current(&self) -> AuthCredential {
        let token = self
            .auth_manager
            .current_or_expired()
            .map(|a| a.key)
            .unwrap_or_default();
        AuthCredential::bearer(token)
    }
    /// Owner identity from the leader's `AuthManager`, so the workspace derives
    /// `WorkspaceIdentity` from this provider instead of a separate auth.json
    /// read. Mirrors the in-process path (`mvp_agent`): prefer `GrokAuth.team_id`
    /// (what shell telemetry/snapshot use) mapped onto a `"Team"` principal so
    /// team attribution is derived; otherwise pass principal fields through.
    /// `None` when no credential is available (identity resolution never blocks).
    fn identity(&self) -> Option<AuthIdentity> {
        let a = self.auth_manager.current_or_expired()?;
        Some(match a.team_id.filter(|t| !t.is_empty()) {
            Some(team) => AuthIdentity {
                user_id: a.user_id,
                principal_type: Some("Team".to_string()),
                principal_id: Some(team),
            },
            None => AuthIdentity {
                user_id: a.user_id,
                principal_type: a.principal_type,
                principal_id: a.principal_id,
            },
        })
    }
}
struct WorkspaceExposure {
    handle: WorkspaceHandle,
    hub_url: String,
    cwd: PathBuf,
    started_at: Instant,
    paused: std::sync::atomic::AtomicBool,
}
/// Rewrite JSON-RPC request ID **in place** by prefixing with client ID to
/// avoid collisions.
///
/// Uses a pipe separator which is valid in JSON but unlikely to appear in
/// typical JSON-RPC IDs (which are usually numbers or UUIDs).
///
/// Only rewrites IDs for **requests** (messages with a "method" field).
/// Responses (messages with "result" or "error" but no "method") are left
/// untouched so the agent can match them to its pending requests.
///
/// Returns `Some((namespaced_id, original_id))` when the message is a request
/// carrying an ID (and `json` was mutated); `None` otherwise (no mutation).
/// The returned `namespaced_id` lets the caller key per-request state (e.g.
/// `pending_load_by_req`) without re-parsing the rewritten payload.
fn rewrite_request_id(
    json: &mut serde_json::Value,
    client_id: ClientId,
) -> Option<(String, serde_json::Value)> {
    json.get("method")?;
    let original_id = json.get("id").cloned()?;
    let original_json = serde_json::to_string(&original_id).unwrap_or_default();
    let namespaced_id = format!("{}{}{}", client_id.0, ID_NAMESPACE_SEP, original_json);
    json["id"] = serde_json::json!(namespaced_id);
    Some((namespaced_id, original_id))
}
/// Parse a namespaced response ID to find the target client, restoring the
/// original ID **in place**.
///
/// Expects format: "client_id|original_id_json" where original_id_json is the
/// JSON-serialized form of the original ID (preserving type information).
///
/// Returns `Some((client_id, namespaced_id))` if successful (`json` now
/// carries the restored original ID). The returned `namespaced_id` is the raw
/// pre-restore ID, so the caller can match per-request state (e.g.
/// `pending_load_by_req`) without re-parsing the original payload. On `None`,
/// `json` is untouched.
fn parse_response_id(json: &mut serde_json::Value) -> Option<(ClientId, String)> {
    let id = json.get("id")?;
    let id_str = id.as_str()?;
    let (client_part, original_json) = id_str.split_once(ID_NAMESPACE_SEP)?;
    let client_id: u64 = client_part.parse().ok()?;
    let original_id: serde_json::Value = serde_json::from_str(original_json).ok()?;
    let namespaced_id = id_str.to_string();
    json["id"] = original_id;
    Some((ClientId(client_id), namespaced_id))
}
/// Extract session_id from a message's params (for session-based routing).
fn extract_session_id(json: &serde_json::Value) -> Option<String> {
    let params = json.get("params")?;
    params
        .get("sessionId")
        .or_else(|| params.get("session_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            params
                .get("params")
                .and_then(|inner| inner.get("sessionId").or_else(|| inner.get("session_id")))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
}
/// Whether a payload is a `session/load` request. Used to start buffering live
/// broadcasts to the loading client until its replay completes (the
/// live-before-replay race, see `load_live_buffer`). Only `session/load`
/// triggers this — `session/new` creators receive everything live correctly.
fn is_session_load_request(json: &serde_json::Value) -> bool {
    json.get("method")
        .and_then(|m| m.as_str())
        .is_some_and(|m| m == "session/load")
}
/// Extract the leader unicast target `ClientId` from a notification's
/// `params._meta["x.ai/leaderClientId"]`.
///
/// The agent stamps this onto every `session/load` replay notification (echoing
/// the id the leader injected into the load request) so the replay can be routed
/// back to ONLY the loading client instead of broadcasting to all subscribers.
/// Live (non-replay) turn deltas are never tagged, so they keep broadcasting.
fn extract_target_client_id(json: &serde_json::Value) -> Option<ClientId> {
    let params = json.get("params")?;
    params
        .get("_meta")
        .and_then(|m| m.get("x.ai/leaderClientId"))
        .or_else(|| {
            params
                .get("params")
                .and_then(|inner| inner.get("_meta"))
                .and_then(|m| m.get("x.ai/leaderClientId"))
        })
        .and_then(|v| v.as_u64())
        .map(ClientId)
}
/// Extract the monotonic `event_seq` counter from a notification's
/// `_meta.eventId` (format `"{sessionId}-{counter}"`). Mirrors the `_meta`
/// lookup in [`extract_target_client_id`] (also checks the ExtNotification
/// `params.params` nesting) and the suffix parse used by
/// `session::storage` and the client's `acp::meta`. Returns `None` for
/// notifications without an `eventId` (xAI one-shots / older shell).
fn event_seq_of(json: &serde_json::Value) -> Option<u64> {
    let params = json.get("params")?;
    let event_id = params
        .get("_meta")
        .and_then(|m| m.get("eventId"))
        .or_else(|| {
            params
                .get("params")
                .and_then(|inner| inner.get("_meta"))
                .and_then(|m| m.get("eventId"))
        })
        .and_then(|v| v.as_str())?;
    event_id.rsplit_once('-')?.1.parse::<u64>().ok()
}
/// Whether a payload is a machine-wide notification (no `sessionId`) that
/// must be **broadcast to every client** instead of falling through to the
/// last-active-client fallback:
///
/// - `x.ai/sessions/changed` — roster delta; every open dashboard must stay
///   in sync.
/// - `x.ai/models/update` — the model catalog changed (config.toml
///   `[model.*]`/`[models]` hot-reload, `models_cache.json` external write,
///   auth change, response-header etag refresh). Every connected client's
///   model picker must refresh, not just the most recently active one.
/// - `x.ai/mcp/servers_updated` — the MCP catalog resolved/changed (managed
///   connectors fetched in the background after `initialize`). Deliberately
///   session-agnostic on the wire (no `sessionId`, see
///   `extensions::mcp::notify_servers_updated`); the push fires seconds after
///   `initialize` returns, so last-active-client fallback routinely delivered
///   it to the wrong client (or dropped it) in multi-client leaders — managed
///   connectors then "disappeared" from every other client's `/mcp` view.
///   Broadcast is safe: the pager handler only debounce-refetches `mcp/list`
///   for agents with an open extensions modal.
/// - `x.ai/announcements/update` — the announcements list changed (startup
///   one-shot or the periodic settings refresh). Session-agnostic; every
///   client renders its own banner, so last-active-client fallback would
///   leave every other client's banner stale. Broadcast is safe: the pager
///   handler is idempotent and drops stale generations via its `gen` gate.
///   (`x.ai/settings/update` stays non-broadcast — it carries auth/gate state.)
///
/// Matched via [`method_of`], NOT the raw top-level `method`: agent ext
/// notifications arrive `_`-prefixed on the wire (`_x.ai/sessions/changed`),
/// so a raw compare would miss the production form.
fn is_machine_wide_broadcast_notification(json: &serde_json::Value) -> bool {
    matches!(
        method_of(json),
        Some(
            "x.ai/sessions/changed"
                | "x.ai/models/update"
                | "x.ai/mcp/servers_updated"
                | "x.ai/announcements/update"
        )
    )
}
/// Whether a payload is the `x.ai/scheduled_task_inject_prompt` notification.
///
/// This notification tells the receiving client to enqueue AND drive a
/// scheduled (`/loop`) cron prompt. Unlike ordinary `sessionId`-bearing
/// notifications (which fan out to every subscriber so each renders an
/// identical stream), it must be routed to the SINGLE session driver: if every
/// attached client received it, each would enqueue + try to drive the same cron
/// turn, duplicating it (phantom `#N` queue entries, competing drivers, stuck
/// turns). The other clients render the resulting turn from the broadcast
/// `session/update` deltas, exactly like any other turn the driver runs.
/// The namespaced method a leader payload carries, normalizing the two ext wire
/// forms the gateway produces:
///   - direct:  `{"method":"x.ai/foo", ...}`                                 -> `x.ai/foo`
///   - wrapped: `{"method":"_x.ai/foo","params":{"method":"x.ai/foo",...}}`  -> `x.ai/foo`
///
/// Gateway-forwarded ext methods/notifications (`ext_method` / `ext_notification`
/// — e.g. `ask_user_question`, `exit_plan_mode`, `scheduled_task_inject_prompt`,
/// `session_notification`) arrive WRAPPED: a top-level `_`-prefixed method with
/// the real method + params nested one level under `params`. Plain methods
/// (`session/request_permission`, `session/update`, …) arrive direct. Anything
/// that classifies a payload by method name MUST use this — matching the raw
/// top-level `method` misses the wrapped form. See `interaction_inner_params`
/// for the matching params accessor.
fn method_of(json: &serde_json::Value) -> Option<&str> {
    let top = json.get("method")?.as_str()?;
    if let Some(stripped) = top.strip_prefix('_') {
        return Some(
            json.get("params")
                .and_then(|p| p.get("method"))
                .and_then(|m| m.as_str())
                .unwrap_or(stripped),
        );
    }
    Some(top)
}
/// The real params object for a payload, unwrapping the gateway ext wrapper:
/// for a wrapped ext (its `params` carries its own `method` + nested `params`)
/// the real params live at `params.params`; otherwise `params` is already real.
fn interaction_inner_params(json: &serde_json::Value) -> Option<&serde_json::Value> {
    let params = json.get("params")?;
    if params.get("method").is_some()
        && let Some(inner) = params.get("params")
    {
        Some(inner)
    } else {
        Some(params)
    }
}
/// Whether a payload is the `x.ai/scheduled_task_inject_prompt` notification.
///
/// This notification tells the receiving client to enqueue AND drive a
/// scheduled (`/loop`) cron prompt. Unlike ordinary `sessionId`-bearing
/// notifications (which fan out to every subscriber so each renders an
/// identical stream), it must be routed to the SINGLE session driver: if every
/// attached client received it, each would enqueue + try to drive the same cron
/// turn, duplicating it (phantom `#N` queue entries, competing drivers, stuck
/// turns). The other clients render the resulting turn from the broadcast
/// `session/update` deltas, exactly like any other turn the driver runs.
fn is_scheduled_task_inject_prompt(json: &serde_json::Value) -> bool {
    method_of(json) == Some("x.ai/scheduled_task_inject_prompt")
}
/// Whether a payload is a blocking *interaction* reverse-request — a tool
/// permission, `ask_user_question`, or plan-approval. Unlike other
/// reverse-requests (driver-only), these are **shared**: broadcast to every
/// subscriber so any client can render + answer the modal, first-answer-wins.
/// See `SHARED_INTERACTIVE_MODALS.md`.
fn is_interaction_request(json: &serde_json::Value) -> bool {
    matches!(
        method_of(json),
        Some("session/request_permission" | "x.ai/ask_user_question" | "x.ai/exit_plan_mode")
    )
}
/// Extract the `tool_call_id` an interaction reverse-request carries, so the
/// leader can cache it (keyed by id) for replay-on-attach and evict it on
/// `InteractionResolved`. The ext-methods (`ask_user_question` /
/// `exit_plan_mode`) carry it directly under (inner) `params`;
/// `request_permission` nests it under `toolCall`. Tolerant of the gateway
/// wrapper (via `interaction_inner_params`) and camel/snake spelling.
fn extract_interaction_tool_call_id(json: &serde_json::Value) -> Option<String> {
    let params = interaction_inner_params(json)?;
    if let Some(id) = params
        .get("toolCallId")
        .or_else(|| params.get("tool_call_id"))
        .and_then(|v| v.as_str())
    {
        return Some(id.to_string());
    }
    let tc = params.get("toolCall").or_else(|| params.get("tool_call"))?;
    tc.get("toolCallId")
        .or_else(|| tc.get("tool_call_id"))
        .or_else(|| tc.get("id"))
        .and_then(|v| v.as_str())
        .map(String::from)
}
/// If a payload is the `InteractionResolved` broadcast (an
/// `x.ai/session_notification` whose `update.sessionUpdate ==
/// "interaction_resolved"`), return its `tool_call_id` so the leader can evict
/// the cached interaction request (first-answer-wins). Tolerant of the gateway
/// wrapper and camel/snake spelling for the inner field.
fn extract_interaction_resolved_tool_call_id(json: &serde_json::Value) -> Option<String> {
    if method_of(json) != Some("x.ai/session_notification") {
        return None;
    }
    let update = interaction_inner_params(json)?.get("update")?;
    if update.get("sessionUpdate").and_then(|v| v.as_str()) != Some("interaction_resolved") {
        return None;
    }
    update
        .get("tool_call_id")
        .or_else(|| update.get("toolCallId"))
        .and_then(|v| v.as_str())
        .map(String::from)
}
/// Extract session_id from a prompt-complete notification.
fn extract_session_id_from_prompt_complete(json: &serde_json::Value) -> Option<String> {
    let method = json.get("method")?.as_str()?;
    if method != "x.ai/session/prompt_complete" {
        return None;
    }
    json.get("params")?
        .get("sessionId")
        .or_else(|| json.get("params")?.get("session_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}
/// Extract session_id from a response's result (for session/new and session/load responses).
/// This is used to track session ownership when the session is first created.
fn extract_session_id_from_result(json: &serde_json::Value) -> Option<String> {
    let result = json.get("result")?;
    result
        .get("session_id")
        .or_else(|| result.get("sessionId"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}
#[derive(Debug)]
enum ChildSessionEvent {
    Spawned(String),
    Finished(String),
}
/// Extract child session lifecycle events from subagent notifications.
fn extract_child_session_event(json: &serde_json::Value) -> Option<ChildSessionEvent> {
    let params = json.get("params")?;
    let update = params
        .get("update")
        .or_else(|| params.get("params")?.get("update"))?;
    let child_sid = update.get("child_session_id").and_then(|v| v.as_str())?;
    match update.get("sessionUpdate")?.as_str()? {
        "subagent_spawned" => Some(ChildSessionEvent::Spawned(child_sid.to_string())),
        "subagent_finished" => Some(ChildSessionEvent::Finished(child_sid.to_string())),
        _ => None,
    }
}
/// Drop a finished child's route + driver and detach it from the child forest,
/// RE-PARENTING any still-live grandchildren onto the finished child's own
/// parent. Re-parenting (not cascade-prune) keeps a still-running grandchild
/// of a finished intermediate reachable from the root: `backfill_child_routes`
/// only follows forward edges, so a subtree left dangling under the removed
/// child would never be reached on a root `session/load`. A genuinely-dead leaf
/// (no surviving children) is simply removed.
///
/// The current parent is found by searching the forest — not taken from the
/// finish notification's sessionId — so a grandchild already re-parented by an
/// earlier intermediate finish is still detached from its correct edge.
fn prune_child_route(
    child_sid: &str,
    session_subscribers: &mut HashMap<String, HashSet<ClientId>>,
    session_driver: &mut HashMap<String, ClientId>,
    child_sessions: &mut HashMap<String, HashSet<String>>,
) {
    session_subscribers.remove(child_sid);
    session_driver.remove(child_sid);
    let parent = child_sessions
        .iter()
        .find_map(|(p, kids)| kids.contains(child_sid).then(|| p.clone()));
    if let Some(ref parent) = parent
        && let Some(kids) = child_sessions.get_mut(parent)
    {
        kids.remove(child_sid);
    }
    if let Some(grandchildren) = child_sessions.remove(child_sid)
        && let Some(ref parent) = parent
    {
        child_sessions
            .entry(parent.clone())
            .or_default()
            .extend(grandchildren);
    }
    if let Some(parent) = parent
        && child_sessions.get(&parent).is_some_and(HashSet::is_empty)
    {
        child_sessions.remove(&parent);
    }
}
/// Subscribe `client` to every live descendant of `parent` (walking the
/// parent→children index, depth-safe via a visited set) and give driverless
/// descendants the parent's driver. Child routes are otherwise spawn-time
/// snapshots, so without this a client that attaches to the parent AFTER a
/// subagent spawned (late attach, reconnect) never receives the child's live
/// updates.
fn backfill_child_routes(
    parent: &str,
    client: ClientId,
    child_sessions: &HashMap<String, HashSet<String>>,
    session_subscribers: &mut HashMap<String, HashSet<ClientId>>,
    session_driver: &mut HashMap<String, ClientId>,
) {
    let parent_driver = session_driver.get(parent).copied();
    let mut stack: Vec<&str> = vec![parent];
    let mut visited: HashSet<&str> = HashSet::new();
    while let Some(sid) = stack.pop() {
        let Some(children) = child_sessions.get(sid) else {
            continue;
        };
        for child in children {
            if !visited.insert(child.as_str()) {
                continue;
            }
            session_subscribers
                .entry(child.clone())
                .or_default()
                .insert(client);
            if let Some(driver) = parent_driver {
                session_driver.entry(child.clone()).or_insert(driver);
            }
            stack.push(child);
        }
    }
}
/// Inject client capabilities into a session/new request, **in place**.
///
/// If the payload is a session/new request:
/// - If the client has yolo_mode enabled, injects `yoloMode: true` into the request's `_meta` object.
/// - If the client has default_model set and the request doesn't already have a modelId,
///   injects `modelId` into the request's `_meta` object.
/// - Injects `clientIdentifier` so the agent can track which client owns each session
///   (used for scoping `yolo_mode_changed` broadcasts in leader mode).
///
/// Returns `true` when `json` was mutated.
fn inject_capabilities_into_session_new(
    json: &mut serde_json::Value,
    capabilities: &ClientCapabilities,
    client_type: &str,
    client_id: ClientId,
) -> bool {
    let has_model = capabilities
        .default_model
        .as_ref()
        .is_some_and(|m| !m.is_empty());
    if !capabilities.yolo_mode
        && !capabilities.auto_mode
        && !has_model
        && client_type.is_empty()
        && !capabilities.code_nav_enabled
    {
        return false;
    }
    let method = json.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let is_session_new = method == AGENT_METHOD_NAMES.session_new;
    let is_session_load = method == AGENT_METHOD_NAMES.session_load;
    if !is_session_new && !is_session_load {
        return false;
    }
    let mut mutated = false;
    if let Some(params) = json.get_mut("params").and_then(|p| p.as_object_mut()) {
        let meta = params
            .entry("_meta")
            .or_insert_with(|| serde_json::json!({}));
        if let Some(meta_obj) = meta.as_object_mut() {
            mutated = true;
            if is_session_new && capabilities.yolo_mode && !meta_obj.contains_key("yoloMode") {
                meta_obj.insert("yoloMode".to_string(), serde_json::json!(true));
                debug!("Injected yoloMode=true into session/new request");
            }
            if capabilities.auto_mode
                && !capabilities.yolo_mode
                && !meta_obj.contains_key("autoMode")
            {
                meta_obj.insert("autoMode".to_string(), serde_json::json!(true));
                debug!("Injected autoMode=true into session request");
            }
            if is_session_new
                && let Some(ref model_id) = capabilities.default_model
                && !model_id.is_empty()
                && !meta_obj.contains_key("modelId")
            {
                meta_obj.insert("modelId".to_string(), serde_json::json!(model_id));
                debug!(model_id, "Injected modelId into session/new request");
            }
            if !client_type.is_empty() && !meta_obj.contains_key("clientIdentifier") {
                meta_obj.insert(
                    "clientIdentifier".to_string(),
                    serde_json::json!(client_type),
                );
            }
            if !meta_obj.contains_key("x.ai/leaderClientId") {
                meta_obj.insert(
                    "x.ai/leaderClientId".to_string(),
                    serde_json::json!(client_id.0),
                );
            }
            meta_obj.insert(
                "codeNavEnabled".to_string(),
                serde_json::json!(capabilities.code_nav_enabled),
            );
            debug!(
                code_nav_enabled = capabilities.code_nav_enabled,
                "Injected codeNavEnabled into session request"
            );
            meta_obj.insert(
                "clientTerminal".to_string(),
                serde_json::json!(capabilities.terminal),
            );
            meta_obj.insert(
                "clientFsRead".to_string(),
                serde_json::json!(capabilities.fs_read),
            );
            meta_obj.insert(
                "clientFsWrite".to_string(),
                serde_json::json!(capabilities.fs_write),
            );
        }
    }
    mutated
}
/// Inject client identity into an `initialize` request.
///
/// In leader mode, multiple clients (TUI, IDE extension, web) share one agent process.
/// The agent's `client_type` is set during `initialize` from `_meta.clientIdentifier`,
/// so the leader injects the IPC registration `client_type` to ensure the agent knows
/// the real client identity.
///
/// Only injects if `clientIdentifier` is not already present in `_meta` — respects
/// explicit client-provided values.
///
/// Mutates `json` in place. Returns `(mutated, was_initialize)`. The second
/// boolean is `true` only when the message was an `initialize` request,
/// allowing the caller to record that `initialize` has been seen.
fn inject_client_identity_into_initialize(
    json: &mut serde_json::Value,
    client_type: &str,
) -> (bool, bool) {
    if client_type.is_empty() {
        return (false, false);
    }
    let is_initialize = json
        .get("method")
        .and_then(|m| m.as_str())
        .is_some_and(|m| m == AGENT_METHOD_NAMES.initialize);
    if !is_initialize {
        return (false, false);
    }
    let mut mutated = false;
    if let Some(params) = json.get_mut("params").and_then(|p| p.as_object_mut()) {
        let meta = params
            .entry("_meta")
            .or_insert_with(|| serde_json::json!({}));
        if let Some(meta_obj) = meta.as_object_mut()
            && !meta_obj.contains_key("clientIdentifier")
        {
            meta_obj.insert(
                "clientIdentifier".to_string(),
                serde_json::json!(client_type),
            );
            mutated = true;
            debug!(
                client_type,
                "Injected clientIdentifier into initialize request"
            );
        }
    }
    (mutated, true)
}
/// Extract yolo_mode change from x.ai/yolo_mode_changed notification.
///
/// Returns Some(yolo_mode) if this is a yolo mode change notification.
fn extract_yolo_mode_change(json: &serde_json::Value) -> Option<bool> {
    let method = json.get("method")?.as_str()?;
    if method != "x.ai/yolo_mode_changed" {
        return None;
    }
    let params = json.get("params")?;
    params.get("yolo_mode").and_then(|v| v.as_bool())
}
/// Extract the auto-mode intent from an `x.ai/yolo_mode_changed` notification, so
/// the leader can keep `ClientCapabilities.auto_mode` fresh the same way it tracks
/// `yolo_mode`. Without this, a stale connect-time `auto_mode` capability would be
/// injected into later `session/new` requests, re-enabling Auto after the user opted
/// out. Returns `None` when the notification doesn't change auto state.
fn extract_auto_mode_change(json: &serde_json::Value) -> Option<bool> {
    let method = json.get("method")?.as_str()?;
    if method != "x.ai/yolo_mode_changed" {
        return None;
    }
    let params = json.get("params")?;
    if let Some(b) = params.get("auto_mode").and_then(|v| v.as_bool()) {
        return Some(b);
    }
    match params.get("permission_mode").and_then(|v| v.as_str()) {
        Some("auto") => Some(true),
        Some("always-approve" | "ask" | "default") => Some(false),
        _ => None,
    }
}
/// Inject `clientIdentifier` into a `yolo_mode_changed` notification's params.
///
/// In leader mode, multiple clients share one agent. Without this injection, the agent
/// can't tell which client sent the yolo toggle and updates ALL sessions. With the
/// `clientIdentifier` in params, the agent scopes the update to only sessions owned
/// by the sending client.
///
/// Mutates `json` in place; returns `true` when mutated.
fn inject_client_identity_into_yolo_notification(
    json: &mut serde_json::Value,
    client_type: &str,
) -> bool {
    if client_type.is_empty() {
        return false;
    }
    let is_yolo = json
        .get("method")
        .and_then(|m| m.as_str())
        .is_some_and(|m| m == "x.ai/yolo_mode_changed");
    if !is_yolo {
        return false;
    }
    let mut mutated = false;
    if let Some(params) = json.get_mut("params").and_then(|p| p.as_object_mut()) {
        params.insert(
            "clientIdentifier".to_string(),
            serde_json::json!(client_type),
        );
        mutated = true;
        debug!(
            client_type,
            "Injected clientIdentifier into yolo_mode_changed notification"
        );
    }
    mutated
}
/// Build a JSON-RPC error response for requests that arrive before the leader is ready.
///
/// Returns `Some(payload)` when the message has an `id` field (i.e. is a request),
/// so the client gets a structured response it can act on instead of hanging.
/// Returns `None` for notifications (no `id`) — those are silently dropped.
fn make_leader_starting_error(json: &serde_json::Value) -> Option<String> {
    let id = json.get("id").filter(|v| !v.is_null()).cloned()?;
    let response = serde_json::json!(
        { "jsonrpc" : "2.0", "id" : id, "error" : { "code" : - 32002, "message" :
        "leader_starting", "data" :
        "Leader is still initializing (auth/prefetch in progress). Retry shortly." } }
    );
    Some(response.to_string())
}
/// Choose the bytes forwarded to the agent: the re-serialized `json` when an
/// injection/rewrite mutated it, the original `payload` verbatim otherwise
/// (including non-JSON payloads, which are never parsed or re-serialized).
fn select_outbound_payload(
    json: Option<&serde_json::Value>,
    payload_mutated: bool,
    payload: String,
) -> String {
    match json {
        Some(j) if payload_mutated => j.to_string(),
        _ => payload,
    }
}
/// Patch the `initialize` response so `meta.modelState.currentModelId` reflects the
/// client's `default_model` instead of the agent's global `current_model_id`.
///
/// Without this the TUI briefly shows the agent's startup default then jumps to the
/// client's preferred model once the first `session/new` response arrives.
///
/// Mutates `json` in place; returns `true` when patched.
fn patch_initialize_response_model(
    json: &mut serde_json::Value,
    default_model: &Option<String>,
) -> bool {
    let Some(model) = default_model.as_ref().filter(|m| !m.is_empty()) else {
        return false;
    };
    let needs_patch = json
        .pointer("/result/meta/modelState/currentModelId")
        .and_then(|v| v.as_str())
        .is_some_and(|current| current != model.as_str());
    if needs_patch {
        json["result"]["meta"]["modelState"]["currentModelId"] =
            serde_json::Value::String(model.clone());
        debug!(patched_model = % model, "Patched initialize response currentModelId");
        return true;
    }
    false
}
/// Extract model ID from a `session/setModel` request (for keeping `default_model` in sync).
fn extract_model_id_from_set_model(json: &serde_json::Value) -> Option<String> {
    let method = json.get("method")?.as_str()?;
    if method != AGENT_METHOD_NAMES.session_set_model {
        return None;
    }
    let params = json.get("params")?;
    params
        .get("modelId")
        .or_else(|| params.get("model_id"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}
fn cpu_profile_status_payload(status: CpuProfileStatus) -> ControlPayload {
    match status {
        CpuProfileStatus::Inactive => ControlPayload::CpuProfileStatus {
            active: false,
            stopping: false,
            started_at: None,
            svg_path: None,
            frequency_hz: None,
        },
        CpuProfileStatus::Active {
            started_at,
            svg_path,
            frequency_hz,
        } => ControlPayload::CpuProfileStatus {
            active: true,
            stopping: false,
            started_at: Some(started_at),
            svg_path: Some(svg_path),
            frequency_hz: Some(frequency_hz),
        },
        CpuProfileStatus::Stopping {
            started_at,
            svg_path,
            frequency_hz,
        } => ControlPayload::CpuProfileStatus {
            active: false,
            stopping: true,
            started_at: Some(started_at),
            svg_path: Some(svg_path),
            frequency_hz: Some(frequency_hz),
        },
    }
}
fn leader_info_payload(control_state: &LeaderServerControlState) -> ControlPayload {
    let manager = control_state.cpu_profile.lock();
    let status = manager.status();
    let (cpu_profile_active, cpu_profile_stopping, profile_started_at) = match status {
        CpuProfileStatus::Inactive => (false, false, None),
        CpuProfileStatus::Active { started_at, .. } => (true, false, Some(started_at)),
        CpuProfileStatus::Stopping { started_at, .. } => (false, true, Some(started_at)),
    };
    ControlPayload::LeaderInfo {
        pid: control_state.metadata.pid,
        socket_path: control_state.metadata.socket_path.clone(),
        lock_path: control_state.metadata.lock_path.clone(),
        ws_url_suffix: control_state.metadata.ws_url_suffix.clone(),
        leader_protocol_version: LEADER_PROTOCOL_VERSION,
        leader_binary_version: control_state.metadata.leader_binary_version.clone(),
        profiling_supported: manager.runtime_cpu_profile(),
        profiling_compiled_in: manager.profiling_compiled_in(),
        cpu_profile_active,
        cpu_profile_stopping,
        profile_started_at,
        profile_formats: manager.profile_formats().to_vec(),
    }
}
const PROD_COMPUTER_HUB_URL: &str = "wss://computer-hub.grok.com/v1/tools";
const WORKSPACE_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
fn workspace_err(message: impl Into<String>) -> ControlError {
    ControlError {
        code: ControlErrorCode::InternalError,
        message: message.into(),
        details: None,
    }
}
/// Resolve the hub credential, waiting if the leader is still wiring auth
/// (the IPC socket comes up first). Resolves the instant auth is wired or the
/// leader cancels — event-driven, no timeout.
async fn wait_for_leader_auth(
    ws: &WorkspaceControl,
    cancel: &CancellationToken,
) -> Result<Arc<dyn AuthProvider>, ControlError> {
    let mut rx = ws.auth.subscribe();
    let result = tokio::select! {
        result = rx.wait_for(| v | v.is_some()) => result, _ = cancel.cancelled() => {
        return
        Err(workspace_err("leader is shutting down; cannot expose workspace to the hub",));
        }
    };
    match result {
        Ok(guard) => Ok(guard.clone().expect("waited for Some")),
        Err(_) => Err(workspace_err(
            "leader is shutting down; cannot expose workspace to the hub",
        )),
    }
}
fn workspace_server_id() -> String {
    let raw = gethostname::gethostname()
        .to_string_lossy()
        .to_ascii_lowercase();
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let name = sanitized.trim_matches('-');
    if name.is_empty() {
        "grok-workspace".to_string()
    } else {
        name.to_string()
    }
}
async fn drain_and_disconnect(handle: &WorkspaceHandle) {
    let tracker = handle.activity_tracker().clone();
    tracker.set_draining();
    if tokio::time::timeout(WORKSPACE_DRAIN_TIMEOUT, tracker.wait_until_drained())
        .await
        .is_err()
    {
        warn!(
            active = tracker.total_active(),
            "workspace drain timed out; disconnecting hub anyway"
        );
    }
    handle.shutdown_hub().await;
}
fn build_workspace_status(
    metadata: &LeaderServerMetadata,
    exposure: Option<&WorkspaceExposure>,
) -> ControlPayload {
    match exposure {
        None => ControlPayload::WorkspaceStatus {
            state: "none".to_string(),
            hub_url: None,
            cwd: None,
            uptime_ms: 0,
            active_tool_calls: 0,
            sessions: Vec::new(),
            pid: metadata.pid,
        },
        Some(exp) => {
            let snapshot = exp.handle.activity_tracker().snapshot();
            let mut sessions = exp.handle.session_ids();
            sessions.sort();
            ControlPayload::WorkspaceStatus {
                state: if exp.paused.load(std::sync::atomic::Ordering::Relaxed) {
                    "paused"
                } else {
                    "running"
                }
                .to_string(),
                hub_url: Some(exp.hub_url.clone()),
                cwd: Some(exp.cwd.display().to_string()),
                uptime_ms: exp.started_at.elapsed().as_millis() as u64,
                active_tool_calls: snapshot.active_tool_calls,
                sessions,
                pid: metadata.pid,
            }
        }
    }
}
async fn handle_workspace_start(
    control_state: LeaderServerControlState,
    hub_url: Option<String>,
    cwd: String,
    cancel: CancellationToken,
) -> Result<ControlPayload, ControlError> {
    let ws = &control_state.workspace;
    let url_str = hub_url
        .filter(|u| !u.trim().is_empty())
        .or_else(|| ws.default_hub_url.clone())
        .unwrap_or_else(|| PROD_COMPUTER_HUB_URL.to_string());
    let url = url::Url::parse(&url_str)
        .map_err(|e| workspace_err(format!("invalid hub url {url_str}: {e}")))?;
    let cwd_path = PathBuf::from(&cwd);
    let _serialize = ws.lock.lock().await;
    if let Some(existing) = ws.exposure.load_full()
        && !existing.paused.load(Ordering::Relaxed)
        && existing.cwd == cwd_path
        && existing.hub_url == url_str
    {
        return Ok(build_workspace_status(
            &control_state.metadata,
            Some(existing.as_ref()),
        ));
    }
    let allow_insecure_ws =
        url.scheme() == "ws" && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    let status_config = xai_grok_workspace::StatusConfig::from_env();
    let alpha_test_key = None;
    let auth = wait_for_leader_auth(ws, &cancel).await?;
    let server_id = workspace_server_id();
    let metadata = serde_json::json!(
        { "source" : "grok-workspace", "hostname" : gethostname::gethostname()
        .to_string_lossy(), "cwd" : cwd_path.display().to_string(), }
    );
    let upload_queue_enabled =
        std::env::var("GROK_WORKSPACE_UPLOAD_QUEUE_ENABLED").as_deref() != Ok("false");
    crate::agent::folder_trust::resolve_and_record(&cwd_path, None, false);
    let project_lsp_trusted = crate::agent::folder_trust::project_scope_allowed(&cwd_path);
    let handle = xai_grok_workspace::connect_local_workspace(
        cwd_path.clone(),
        url,
        auth,
        Some(metadata),
        Some(server_id),
        alpha_test_key,
        allow_insecure_ws,
        status_config,
        upload_queue_enabled,
        project_lsp_trusted,
        None,
        false,
        false,
    )
    .await
    .map_err(|e| workspace_err(format!("failed to connect workspace to hub: {e}")))?;
    let exposure = Arc::new(WorkspaceExposure {
        handle,
        hub_url: url_str,
        cwd: cwd_path,
        started_at: Instant::now(),
        paused: AtomicBool::new(false),
    });
    let payload = build_workspace_status(&control_state.metadata, Some(exposure.as_ref()));
    if let Some(old) = ws.exposure.swap(Some(exposure)) {
        drain_and_disconnect(&old.handle).await;
    }
    Ok(payload)
}
async fn handle_workspace_pause(
    control_state: LeaderServerControlState,
) -> Result<ControlPayload, ControlError> {
    let ws = &control_state.workspace;
    let _serialize = ws.lock.lock().await;
    let Some(exp) = ws.exposure.load_full() else {
        return Err(workspace_err("no workspace exposure is running"));
    };
    if !exp.paused.load(Ordering::Relaxed) {
        drain_and_disconnect(&exp.handle).await;
        exp.paused.store(true, Ordering::Relaxed);
    }
    Ok(build_workspace_status(
        &control_state.metadata,
        Some(exp.as_ref()),
    ))
}
async fn handle_workspace_resume(
    control_state: LeaderServerControlState,
) -> Result<ControlPayload, ControlError> {
    let ws = &control_state.workspace;
    let _serialize = ws.lock.lock().await;
    let Some(exp) = ws.exposure.load_full() else {
        return Err(workspace_err("no workspace exposure is running"));
    };
    if exp.paused.load(Ordering::Relaxed) {
        exp.handle.activity_tracker().set_active();
        if let Err(e) = exp.handle.connect_hub().await {
            exp.handle.activity_tracker().set_draining();
            return Err(workspace_err(format!("failed to reconnect to hub: {e}")));
        }
        exp.paused.store(false, Ordering::Relaxed);
    }
    Ok(build_workspace_status(
        &control_state.metadata,
        Some(exp.as_ref()),
    ))
}
async fn handle_workspace_stop(
    control_state: LeaderServerControlState,
) -> Result<ControlPayload, ControlError> {
    let ws = &control_state.workspace;
    let _serialize = ws.lock.lock().await;
    if let Some(exp) = ws.exposure.swap(None) {
        drain_and_disconnect(&exp.handle).await;
    }
    Ok(build_workspace_status(&control_state.metadata, None))
}
async fn handle_workspace_status(
    control_state: LeaderServerControlState,
) -> Result<ControlPayload, ControlError> {
    let exposure = control_state.workspace.exposure.load_full();
    Ok(build_workspace_status(
        &control_state.metadata,
        exposure.as_deref(),
    ))
}
async fn finalize_workspace_on_shutdown(control_state: LeaderServerControlState) {
    let ws = &control_state.workspace;
    let _serialize = ws.lock.lock().await;
    if let Some(exp) = ws.exposure.swap(None) {
        info!("Draining workspace exposure on leader shutdown");
        drain_and_disconnect(&exp.handle).await;
    }
}
fn handle_control_command(
    control_state: &LeaderServerControlState,
    command: ControlCommand,
) -> Result<ControlPayload, ControlError> {
    match command {
        ControlCommand::GetLeaderInfo => Ok(leader_info_payload(control_state)),
        ControlCommand::CpuProfileStatus => {
            let manager = control_state.cpu_profile.lock();
            Ok(cpu_profile_status_payload(manager.status()))
        }
        ControlCommand::StartCpuProfile {
            output,
            frequency_hz,
        } => {
            let mut manager = control_state.cpu_profile.lock();
            let status = manager.start(CpuProfileStartOptions {
                output: output.map(PathBuf::from),
                frequency_hz,
            })?;
            match status {
                CpuProfileStatus::Active {
                    started_at,
                    svg_path,
                    frequency_hz,
                } => Ok(ControlPayload::CpuProfileStarted {
                    pid: control_state.metadata.pid,
                    svg_path,
                    frequency_hz,
                    started_at,
                }),
                CpuProfileStatus::Inactive => {
                    Ok(cpu_profile_status_payload(CpuProfileStatus::Inactive))
                }
                CpuProfileStatus::Stopping {
                    started_at,
                    svg_path,
                    frequency_hz,
                } => Ok(cpu_profile_status_payload(CpuProfileStatus::Stopping {
                    started_at,
                    svg_path,
                    frequency_hz,
                })),
            }
        }
        ControlCommand::StopCpuProfile => {
            unreachable!("StopCpuProfile must be handled asynchronously")
        }
        ControlCommand::WorkspaceStart { .. }
        | ControlCommand::WorkspacePause
        | ControlCommand::WorkspaceResume
        | ControlCommand::WorkspaceStop
        | ControlCommand::WorkspaceStatus => {
            unreachable!("workspace control commands are handled asynchronously")
        }
        ControlCommand::RelaunchForUpdate { .. } => {
            unreachable!("RelaunchForUpdate must be handled asynchronously")
        }
    }
}
async fn handle_stop_cpu_profile(
    control_state: LeaderServerControlState,
) -> Result<ControlPayload, ControlError> {
    let stop_handle = {
        let mut manager = control_state.cpu_profile.lock();
        manager.take_stop_handle()?
    };
    let pid = control_state.metadata.pid;
    let result = tokio::task::spawn_blocking(move || stop_handle.finish()).await;
    control_state.cpu_profile.lock().complete_stop();
    let result = result.map_err(|join_error| ControlError {
        code: ControlErrorCode::InternalError,
        message: "CPU profile stop task failed".to_string(),
        details: Some(serde_json::json!({ "error" : join_error.to_string() })),
    })??;
    Ok(ControlPayload::CpuProfileStopped {
        pid,
        svg_path: result.svg_path,
        started_at: result.started_at,
        stopped_at: result.stopped_at,
    })
}
async fn finalize_cpu_profile_on_shutdown(control_state: LeaderServerControlState) {
    let (disposition, stop_handle, mut stop_completion_rx) = {
        let mut manager = control_state.cpu_profile.lock();
        let disposition = manager.shutdown_stop_disposition();
        let stop_completion_rx = manager.subscribe_stop_completion();
        let stop_handle = match manager.take_shutdown_stop_handle() {
            Ok(stop_handle) => stop_handle,
            Err(error) => {
                warn!(
                    error = % error,
                    "Failed to prepare active CPU profile for leader shutdown"
                );
                return;
            }
        };
        (disposition, stop_handle, stop_completion_rx)
    };
    let Some(disposition) = disposition else {
        return;
    };
    let Some(stop_handle) = stop_handle else {
        match disposition {
            ShutdownStopDisposition::AlreadyStopping => {
                debug!(
                    "CPU profile stop already in progress during leader shutdown; waiting for in-flight finalization task"
                );
                if !*stop_completion_rx.borrow() && stop_completion_rx.changed().await.is_err() {
                    warn!("CPU profile stop completion channel closed during leader shutdown wait");
                }
            }
            ShutdownStopDisposition::StartedShutdownStop => {
                warn!("Expected shutdown CPU profile stop handle but none was available");
            }
        }
        return;
    };
    let result = tokio::task::spawn_blocking(move || stop_handle.finish()).await;
    control_state.cpu_profile.lock().complete_stop();
    match result {
        Ok(Ok(result)) => {
            info!(
                path = % result.svg_path.display(), started_at = % result.started_at,
                stopped_at = % result.stopped_at,
                "Finalized active CPU profile during leader shutdown"
            );
        }
        Ok(Err(error)) => {
            warn!(
                error = % error,
                "Failed to finalize active CPU profile during leader shutdown"
            );
        }
        Err(join_error) => {
            warn!(error = % join_error, "CPU profile shutdown finalization task failed");
        }
    }
}
/// Bounded grace the leader waits for in-flight turns to finish before a
/// `RelaunchForUpdate` relaunch. If the agent is still busy when this elapses,
/// the leader exits anyway — the in-flight turn ends and the session reloads
/// cleanly (truncated at the last persisted boundary).
const RELAUNCH_GRACE: Duration = Duration::from_secs(5);
/// Bound on the post-drain session flush ([`AgentActivity::flush_all_sessions`]).
const RELAUNCH_FLUSH_GRACE: Duration = Duration::from_secs(5);
/// Total shutdown budget advertised to clients in the `Relaunching` ack:
/// idle-drain plus session flush.
const RELAUNCH_TOTAL_GRACE: Duration =
    Duration::from_millis((RELAUNCH_GRACE.as_millis() + RELAUNCH_FLUSH_GRACE.as_millis()) as u64);
/// Poll cadence while waiting for the agent to go idle during the grace period.
const RELAUNCH_GRACE_POLL: Duration = Duration::from_millis(100);
/// Decide whether a [`ControlCommand::RelaunchForUpdate`] is accepted (the
/// synchronous half — kept separate from arming the drain so the caller can send
/// the `Relaunching` ack BEFORE the leader begins shutting down; otherwise an
/// idle leader can race the ack and the client sees a dropped control response).
///
/// Declines unless the target is strictly newer (directional guard) and no
/// relaunch is already in progress (idempotent across multiple clients). On
/// accept it sets `relaunching` so duplicate requests are declined.
fn decide_relaunch_for_update(
    control_state: &LeaderServerControlState,
    to_version: String,
    relaunching: &AtomicBool,
) -> Result<ControlPayload, ControlError> {
    let leader_version = control_state.metadata.leader_binary_version.clone();
    if !super::leader_is_older_than(&leader_version, &to_version) {
        debug!(
            from_version = % leader_version, to_version = % to_version,
            "RelaunchForUpdate declined: target is not strictly newer (or unparseable)"
        );
        return Ok(ControlPayload::RelaunchDeclined {
            reason: format!("leader version {leader_version} is not older than {to_version}"),
        });
    }
    if relaunching.swap(true, Ordering::SeqCst) {
        return Ok(ControlPayload::RelaunchDeclined {
            reason: "a relaunch is already in progress".to_string(),
        });
    }
    info!(
        from_version = % leader_version, to_version = % to_version, grace_ms =
        RELAUNCH_TOTAL_GRACE.as_millis() as u64,
        "RelaunchForUpdate accepted; draining before relaunch onto new binary"
    );
    Ok(ControlPayload::Relaunching {
        from_version: leader_version,
        to_version,
        grace_ms: RELAUNCH_TOTAL_GRACE.as_millis() as u64,
    })
}
/// Arm the bounded-grace drain for an accepted relaunch: wait up to
/// [`RELAUNCH_GRACE`] for the agent to go idle (`agent_busy` for IPC traffic
/// AND [`AgentActivity::is_busy`] for relay-driven turns / subagents), flush
/// every session actor, then set [`ShutdownReason::AutoUpdate`] and cancel —
/// the same exit path the auto-update checker uses. Must be called *after*
/// the `Relaunching` ack has been sent so the ack is delivered before
/// `ShuttingDown`.
fn spawn_relaunch_drain(
    shutdown_tx: watch::Sender<super::protocol::ShutdownReason>,
    cancel: CancellationToken,
    agent_busy: Arc<AtomicBool>,
    agent_activity: AgentActivity,
) {
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + RELAUNCH_GRACE;
        while agent_busy.load(Ordering::Relaxed) || agent_activity.is_busy() {
            if tokio::time::Instant::now() >= deadline {
                warn!(
                    "RelaunchForUpdate grace elapsed while agent busy; relaunching anyway (in-flight turn ends)"
                );
                break;
            }
            tokio::select! {
                _ = cancel.cancelled() => return, _ =
                tokio::time::sleep(RELAUNCH_GRACE_POLL) => {}
            }
        }
        agent_activity
            .flush_all_sessions(RELAUNCH_FLUSH_GRACE)
            .await;
        let _ = shutdown_tx.send(super::protocol::ShutdownReason::AutoUpdate);
        cancel.cancel();
    });
}
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("Failed to acquire leader lock: {0}")]
    LockFailed(#[from] super::lock::LockError),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
/// Build the ACP notification payload for a leader/client version mismatch, or
/// return `None` when versions match or detection is disabled.
///
/// Extracted as a standalone function so the notification shape can be unit-tested
/// without running a full server.
fn make_version_mismatch_notification(
    client_version: &str,
    leader_version: &str,
) -> Option<String> {
    if client_version == leader_version || leader_version == "unknown" {
        return None;
    }
    Some(
        serde_json::json!(
            { "jsonrpc" : "2.0", "method" : "x.ai/leader/version_mismatch", "params" : {
            "clientVersion" : client_version, "leaderVersion" : leader_version, "message"
            :
            format!("Client version {client_version} differs from leader version \
                     {leader_version}. Restart the grok binary to use the same version.")
            } }
        )
        .to_string(),
    )
}
/// Run the leader IPC server.
///
/// The socket_path is where the Unix socket will be created.
/// Caller is responsible for:
/// 1. Cleaning up any stale socket file before calling this
/// 2. Acquiring the leader lock AFTER this function creates the socket
///
/// This ordering ensures that:
/// - Clients waiting for socket can connect as soon as we're ready
/// - The lock acquisition happens after we're actually listening
///
/// # Readiness gating
///
/// The `ready_rx` watch channel controls whether ACP messages are forwarded to the
/// agent. While `*ready_rx.borrow() == false` (leader still initializing):
/// - Client connections and IPC registrations are accepted normally.
/// - ACP requests (messages with an `id`) receive a structured `leader_starting`
///   JSON-RPC error so the client can retry rather than hang.
/// - ACP notifications (no `id`) are dropped with a trace log.
///
/// Once `ready_rx` is signaled `true` (auth + prefetch complete), all subsequent
/// ACP traffic is forwarded to the agent as normal.
///
/// # Arguments
///
/// * `socket_path` - Path for the Unix domain socket
/// * `acp_tx` - Channel to send ACP messages from clients to the agent
/// * `response_rx` - Channel to receive responses from the agent to route to clients
/// * `cancel` - Cancellation token for graceful shutdown
/// * `no_exit_on_disconnect` - If true, don't exit when all clients disconnect
/// * `client_count` - Atomic counter tracking the number of connected clients
/// * `agent_busy` - Atomic flag set while the agent has in-flight **IPC**
///   requests; relay-driven traffic never sets it
/// * `agent_activity` - Agent-derived activity view (running turns, parked
///   interactions, live subagents) consulted by the `RelaunchForUpdate` drain
///   alongside `agent_busy`, plus the pre-shutdown session flush
/// * `ready_rx` - Watch receiver; ACP forwarding is gated until this is `true`
/// * `relay_demand_tx` - Watch sender flipped to `true` when the first
///   [`ClientMode::Headless`] client registers. `run_leader` defers starting the
///   grok.com WebSocket relay until this fires, so a leader serving only
///   interactive clients (TUI dashboard, IDE) never duplicates its ACP stream
///   onto the relay. Headless registration is the devbox-flow marker: those
///   clients are driven remotely *through* the relay.
/// * `shutdown_tx` - Watch sender for the shutdown reason. The server subscribes
///   its own receiver and reads it once when `cancel` fires (defaults to
///   [`ShutdownReason::Manual`]). The auto-update checker and the
///   [`ControlCommand::RelaunchForUpdate`] handler send [`ShutdownReason::AutoUpdate`]
///   before cancelling so clients see the real reason; senders must write before
///   cancelling.
/// * `leader_version_override` - If `Some`, overrides [`LEADER_VERSION`] for version
///   mismatch detection. Pass `None` in production; pass a test version string in
///   integration tests to bypass the `"unknown"` constant that appears in dev builds
///   where `VERSION_WITH_COMMIT` is not set.
/// * `control_state` - Leader-local control metadata and CPU profiling state
pub async fn run_leader_server(
    socket_path: std::path::PathBuf,
    acp_tx: mpsc::UnboundedSender<String>,
    mut response_rx: mpsc::UnboundedReceiver<String>,
    cancel: CancellationToken,
    no_exit_on_disconnect: bool,
    client_count: Arc<AtomicUsize>,
    agent_busy: Arc<AtomicBool>,
    agent_activity: AgentActivity,
    ready_rx: watch::Receiver<bool>,
    relay_demand_tx: watch::Sender<bool>,
    shutdown_tx: watch::Sender<super::protocol::ShutdownReason>,
    leader_version_override: Option<&'static str>,
    control_state: LeaderServerControlState,
) -> Result<(), ServerError> {
    let _ = std::fs::remove_file(&socket_path);
    let shutdown_reason_rx = shutdown_tx.subscribe();
    let listener = LeaderListener::bind(&socket_path)?;
    info!("Leader server listening");
    let (event_tx, event_rx) = kanal::unbounded_async::<ServerEvent>();
    let mut clients: HashMap<ClientId, ClientState> = HashMap::new();
    let mut session_driver: HashMap<String, ClientId> = HashMap::new();
    let mut session_subscribers: HashMap<String, std::collections::HashSet<ClientId>> =
        HashMap::new();
    let mut child_sessions: HashMap<String, HashSet<String>> = HashMap::new();
    let mut pending_load_by_req: HashMap<String, (ClientId, String)> = HashMap::new();
    let mut load_live_buffer: HashMap<(ClientId, String), Vec<BufferedLive>> = HashMap::new();
    let mut orphan_replay_warned: HashSet<ClientId> = HashSet::new();
    let mut load_replay_max_seq: HashMap<(ClientId, String), u64> = HashMap::new();
    let mut interaction_requests: HashMap<String, HashMap<String, Arc<str>>> = HashMap::new();
    let mut last_active_client: Option<ClientId> = None;
    let mut had_clients = false;
    let mut pending_requests: usize = 0;
    let relaunching = Arc::new(AtomicBool::new(false));
    loop {
        let poll = tokio::select! {
            biased; _ = cancel.cancelled() => LeaderServerPoll::Cancelled, accept_result
            = listener.accept() => { LeaderServerPoll::Accept(accept_result.map(|
            (stream, _) | stream)) } Ok(event) = event_rx.recv() =>
            LeaderServerPoll::Event(event), Some(payload) = response_rx.recv() =>
            LeaderServerPoll::Response(payload),
        };
        match poll {
            LeaderServerPoll::Cancelled => {
                let reason = shutdown_reason_rx.borrow().clone();
                info!(?reason, "Leader server shutting down (cancelled)");
                if pending_requests > 0 {
                    debug!(pending_requests, "Resetting agent_busy on shutdown");
                    agent_busy.store(false, Ordering::Relaxed);
                }
                broadcast_shutdown(&clients, reason).await;
                break;
            }
            LeaderServerPoll::Accept(accept_result) => match accept_result {
                Ok(stream) => {
                    had_clients = true;
                    let client_id = ClientId::new();
                    let (tx, rx) = kanal::unbounded_async();
                    clients.insert(
                        client_id,
                        ClientState {
                            tx,
                            mode: ClientMode::Stdio,
                            capabilities: ClientCapabilities::default(),
                            client_type: String::new(),
                            initialize_seen: false,
                            patch_initialize_model: false,
                            registered: false,
                        },
                    );
                    spawn_client_handler(
                        client_id,
                        stream,
                        rx,
                        event_tx.clone(),
                        cancel.child_token(),
                        ready_rx.clone(),
                        control_state.clone(),
                    );
                }
                Err(e) => error!(error = % e, "Accept failed"),
            },
            LeaderServerPoll::Event(event) => match event {
                ServerEvent::Registered(id, mode, capabilities, client_type) => {
                    if let Some(client) = clients.get_mut(&id) {
                        client.mode = mode;
                        client.capabilities = capabilities;
                        client.client_type = client_type;
                        client.registered = true;
                        client_count.fetch_add(1, Ordering::Relaxed);
                        debug!(
                            client_id = id.0, ? mode, yolo_mode = client.capabilities
                            .yolo_mode, client_type = % client.client_type,
                            "Client registered"
                        );
                        xai_grok_telemetry::unified_log::info(
                            "leader.client.registered",
                            None,
                            Some(serde_json::json!(
                                { "client_id" : id.0, "client_type" : client.client_type, }
                            )),
                        );
                        if mode == ClientMode::Headless {
                            let newly_demanded = relay_demand_tx.send_if_modified(|demanded| {
                                let changed = !*demanded;
                                *demanded = true;
                                changed
                            });
                            if newly_demanded {
                                info!(
                                    client_id = id.0,
                                    "First headless client registered; signalling relay demand"
                                );
                            }
                        }
                        let effective_leader_version =
                            leader_version_override.unwrap_or(LEADER_VERSION);
                        if let Some(ref cv) = client.capabilities.client_version
                            && let Some(payload) = make_version_mismatch_notification(
                                cv.as_str(),
                                effective_leader_version,
                            )
                        {
                            warn!(
                                client_id = id.0,
                                client_version = cv.as_str(),
                                leader_version = effective_leader_version,
                                "Version mismatch: client binary differs from leader binary"
                            );
                            let _ = client.tx.try_send(ClientOutbound::Acp(payload.into()));
                        }
                    }
                }
                ServerEvent::Disconnected(id) => {
                    let was_registered = clients.get(&id).is_some_and(|c| c.registered);
                    clients.remove(&id);
                    if was_registered {
                        client_count.fetch_sub(1, Ordering::Relaxed);
                        xai_grok_telemetry::unified_log::info(
                            "leader.client.disconnected",
                            None,
                            Some(serde_json::json!({ "client_id" : id.0 })),
                        );
                    }
                    pending_load_by_req.retain(|_, (c, _)| *c != id);
                    load_live_buffer.retain(|(c, _), _| *c != id);
                    load_replay_max_seq.retain(|(c, _), _| *c != id);
                    let mut detached_sessions: Vec<String> = Vec::new();
                    let viewed: Vec<String> = session_subscribers
                        .iter()
                        .filter(|(_, subs)| subs.contains(&id))
                        .map(|(sid, _)| sid.clone())
                        .collect();
                    for sid in viewed {
                        let now_empty = if let Some(subs) = session_subscribers.get_mut(&sid) {
                            subs.remove(&id);
                            subs.is_empty()
                        } else {
                            true
                        };
                        if now_empty {
                            session_subscribers.remove(&sid);
                            session_driver.remove(&sid);
                            detached_sessions.push(sid);
                        } else if session_driver.get(&sid) == Some(&id) {
                            if let Some(&next) =
                                session_subscribers.get(&sid).and_then(|s| s.iter().next())
                            {
                                session_driver.insert(sid.clone(), next);
                                debug!(
                                    session_id = % sid, old_driver = id.0, new_driver = next.0,
                                    "Transferred session driver after disconnect"
                                );
                            } else {
                                session_driver.remove(&sid);
                            }
                        }
                    }
                    if last_active_client == Some(id) {
                        last_active_client = None;
                    }
                    if !detached_sessions.is_empty() {
                        let evict_notification = serde_json::json!(
                            { "jsonrpc" : "2.0", "method" :
                            "x.ai/internal/evict_sessions", "params" : { "sessionIds" :
                            detached_sessions } }
                        );
                        let _ = acp_tx.send(evict_notification.to_string());
                        info!(
                            client_id = id.0,
                            session_count = detached_sessions.len(),
                            "Sent client-disconnect detach notification for disconnected client"
                        );
                    }
                    debug!(client_id = id.0, "Client removed");
                    if clients.is_empty() && had_clients && !no_exit_on_disconnect {
                        info!("Leader server shutting down (all clients disconnected)");
                        break;
                    }
                }
                ServerEvent::Message(
                    id,
                    ClientMessage::Control {
                        request_id,
                        command,
                    },
                ) => {
                    if let Some(client) = clients.get(&id) {
                        let client_tx = client.tx.clone();
                        let control_state = control_state.clone();
                        let cancel = cancel.clone();
                        let shutdown_tx = shutdown_tx.clone();
                        let agent_busy = agent_busy.clone();
                        let agent_activity = agent_activity.clone();
                        let relaunching = relaunching.clone();
                        tokio::spawn(async move {
                            let result = match command {
                                ControlCommand::StopCpuProfile => {
                                    handle_stop_cpu_profile(control_state).await
                                }
                                ControlCommand::WorkspaceStart { hub_url, cwd } => {
                                    handle_workspace_start(
                                        control_state,
                                        hub_url,
                                        cwd,
                                        cancel.clone(),
                                    )
                                    .await
                                }
                                ControlCommand::WorkspacePause => {
                                    handle_workspace_pause(control_state).await
                                }
                                ControlCommand::WorkspaceResume => {
                                    handle_workspace_resume(control_state).await
                                }
                                ControlCommand::WorkspaceStop => {
                                    handle_workspace_stop(control_state).await
                                }
                                ControlCommand::WorkspaceStatus => {
                                    handle_workspace_status(control_state).await
                                }
                                ControlCommand::RelaunchForUpdate { to_version } => {
                                    decide_relaunch_for_update(
                                        &control_state,
                                        to_version,
                                        &relaunching,
                                    )
                                }
                                other => handle_control_command(&control_state, other),
                            };
                            let arm_relaunch =
                                matches!(result, Ok(ControlPayload::Relaunching { .. }));
                            if let Err(e) = client_tx
                                .send(ServerMessage::ControlResult { request_id, result }.into())
                                .await
                            {
                                warn!(
                                    client_id = id.0, error = % e,
                                    "Failed to send control response to client"
                                );
                            }
                            if arm_relaunch {
                                spawn_relaunch_drain(
                                    shutdown_tx,
                                    cancel,
                                    agent_busy,
                                    agent_activity,
                                );
                            }
                        });
                    }
                }
                ServerEvent::Message(id, ClientMessage::Acp { payload }) => {
                    let mut json: Option<serde_json::Value> = serde_json::from_str(&payload).ok();
                    let mut payload_mutated = false;
                    if !*ready_rx.borrow() {
                        if let Some(error_payload) =
                            json.as_ref().and_then(make_leader_starting_error)
                        {
                            if let Some(client) = clients.get(&id) {
                                let _ = client
                                    .tx
                                    .try_send(ClientOutbound::Acp(error_payload.into()));
                            }
                            trace!(
                                client_id = id.0,
                                "Returned leader_starting error (not yet ready)"
                            );
                        } else {
                            trace!(
                                client_id = id.0,
                                "Dropped pre-ready notification (leader not yet ready)"
                            );
                        }
                        continue;
                    }
                    if let Some(client) = clients.get(&id)
                        && client.mode == ClientMode::Stdio
                    {
                        last_active_client = Some(id);
                    }
                    if let Some(session_id) = json.as_ref().and_then(extract_session_id) {
                        session_subscribers
                            .entry(session_id.clone())
                            .or_default()
                            .insert(id);
                        session_driver.entry(session_id.clone()).or_insert(id);
                        backfill_child_routes(
                            &session_id,
                            id,
                            &child_sessions,
                            &mut session_subscribers,
                            &mut session_driver,
                        );
                    }
                    if let (Some(json), Some(client)) = (json.as_ref(), clients.get_mut(&id)) {
                        if let Some(yolo_mode) = extract_yolo_mode_change(json) {
                            client.capabilities.yolo_mode = yolo_mode;
                            debug!(
                                client_id = id.0,
                                yolo_mode, "Updated client yolo_mode from notification"
                            );
                        }
                        if let Some(auto_mode) = extract_auto_mode_change(json) {
                            client.capabilities.auto_mode = auto_mode;
                            debug!(
                                client_id = id.0,
                                auto_mode, "Updated client auto_mode from notification"
                            );
                        }
                        if let Some(new_model) = extract_model_id_from_set_model(json) {
                            debug!(
                                client_id = id.0, model = % new_model,
                                "Updated client default_model from session/setModel"
                            );
                            client.capabilities.default_model = Some(new_model);
                        }
                    }
                    if let (Some(json), Some(client)) = (json.as_mut(), clients.get_mut(&id)) {
                        if !client.initialize_seen {
                            let (injected, was_initialize) =
                                inject_client_identity_into_initialize(json, &client.client_type);
                            payload_mutated |= injected;
                            if was_initialize {
                                client.initialize_seen = true;
                                if client
                                    .capabilities
                                    .default_model
                                    .as_ref()
                                    .is_some_and(|m| !m.is_empty())
                                {
                                    client.patch_initialize_model = true;
                                }
                            }
                        }
                        payload_mutated |= inject_capabilities_into_session_new(
                            json,
                            &client.capabilities,
                            &client.client_type,
                            id,
                        );
                        payload_mutated |= inject_client_identity_into_yolo_notification(
                            json,
                            &client.client_type,
                        );
                    }
                    let rewritten = json.as_mut().and_then(|j| rewrite_request_id(j, id));
                    payload_mutated |= rewritten.is_some();
                    if let Some(json) = json.as_ref()
                        && is_session_load_request(json)
                        && let Some(load_sid) = extract_session_id(json)
                        && let Some((ns_id, _)) = rewritten.as_ref()
                    {
                        pending_load_by_req.insert(ns_id.clone(), (id, load_sid.clone()));
                        load_live_buffer.entry((id, load_sid)).or_default();
                    }
                    if rewritten.is_some() {
                        pending_requests += 1;
                        agent_busy.store(true, Ordering::Relaxed);
                    }
                    let outbound = select_outbound_payload(json.as_ref(), payload_mutated, payload);
                    let _ = acp_tx.send(outbound);
                }
                ServerEvent::Message(_, _) => {}
            },
            LeaderServerPoll::Response(payload) => {
                let mut json: Option<serde_json::Value> = serde_json::from_str(&payload).ok();
                let parsed_response = json.as_mut().and_then(parse_response_id);
                if parsed_response.is_some() {
                    pending_requests = pending_requests.saturating_sub(1);
                    agent_busy.store(pending_requests > 0, Ordering::Relaxed);
                }
                if let Some((orphan_client, ref orphan_req_id)) = parsed_response
                    && !clients.contains_key(&orphan_client)
                {
                    warn!(
                        client_id = orphan_client.0,
                        request_id = orphan_req_id.as_str(),
                        "Dropping RPC response: requesting client disconnected (response orphaned)"
                    );
                    xai_grok_telemetry::unified_log::warn(
                        "leader.response.orphaned",
                        None,
                        Some(serde_json::json!(
                            { "client_id" : orphan_client.0, "request_id" :
                            orphan_req_id, }
                        )),
                    );
                }
                if let Some((client_id, ref raw_response_id)) = parsed_response
                    && let Some(client) = clients.get_mut(&client_id)
                    && let Some(json) = json.as_mut()
                {
                    if let Some(session_id) = extract_session_id_from_result(json) {
                        session_subscribers
                            .entry(session_id.clone())
                            .or_default()
                            .insert(client_id);
                        session_driver
                            .entry(session_id.clone())
                            .or_insert(client_id);
                        backfill_child_routes(
                            &session_id,
                            client_id,
                            &child_sessions,
                            &mut session_subscribers,
                            &mut session_driver,
                        );
                        trace!(
                            client_id = client_id.0,
                            session_id, "Subscribed client to session from response"
                        );
                    }
                    if client.patch_initialize_model {
                        client.patch_initialize_model = false;
                        patch_initialize_response_model(json, &client.capabilities.default_model);
                    }
                    let restored_payload: Arc<str> = json.to_string().into();
                    match client.tx.try_send(ClientOutbound::Acp(restored_payload)) {
                        Ok(true) => {
                            trace!(client_id = client_id.0, "Routed response via request ID");
                        }
                        Ok(false) => {
                            warn!(
                                client_id = client_id.0,
                                "Failed to send response to client (channel full)"
                            );
                            xai_grok_telemetry::unified_log::warn(
                                "leader.response.send_failed",
                                None,
                                Some(serde_json::json!(
                                    { "client_id" : client_id.0, "reason" : "channel_full", }
                                )),
                            );
                        }
                        Err(e) => {
                            warn!(
                                client_id = client_id.0, error = % e,
                                "Failed to send response to client (channel closed)"
                            );
                            xai_grok_telemetry::unified_log::warn(
                                "leader.response.send_failed",
                                None,
                                Some(serde_json::json!(
                                    { "client_id" : client_id.0, "reason" : "channel_closed", }
                                )),
                            );
                        }
                    }
                    if let Some((buf_client, buf_sid)) = pending_load_by_req.remove(raw_response_id)
                    {
                        let replay_cutoff: Option<u64> =
                            load_replay_max_seq.remove(&(buf_client, buf_sid.clone()));
                        if let Some(buffered) =
                            load_live_buffer.remove(&(buf_client, buf_sid.clone()))
                            && let Some(target) = clients.get(&buf_client)
                        {
                            let mut count = 0usize;
                            let mut deduped = 0usize;
                            for (buffered_payload, buffered_seq) in buffered {
                                if let Some(cutoff) = replay_cutoff
                                    && buffered_seq.is_some_and(|s| s <= cutoff)
                                {
                                    deduped += 1;
                                    continue;
                                }
                                if let Err(e) =
                                    target.tx.try_send(ClientOutbound::Acp(buffered_payload))
                                {
                                    warn!(
                                        client_id = buf_client.0, error = % e,
                                        "Failed to flush buffered live notification after load (channel closed)"
                                    );
                                    break;
                                }
                                count += 1;
                            }
                            if count > 0 || deduped > 0 {
                                trace!(
                                    client_id = buf_client.0,
                                    count,
                                    deduped,
                                    "Flushed buffered live notifications after load (replay-overlap dropped)"
                                );
                            }
                        }
                        if let Some(cached) = interaction_requests.get(buf_sid.as_str())
                            && let Some(target) = clients.get(&buf_client)
                        {
                            let count = cached.len();
                            for req in cached.values() {
                                if let Err(e) = target.tx.try_send(ClientOutbound::Acp(req.clone()))
                                {
                                    warn!(
                                        client_id = buf_client.0, error = % e,
                                        "Failed to replay interaction request after load (channel closed)"
                                    );
                                    break;
                                }
                            }
                            if count > 0 {
                                trace!(
                                    client_id = buf_client.0,
                                    count,
                                    session_id = buf_sid.as_str(),
                                    "Replayed pending interaction modals to newly-attached client"
                                );
                            }
                        }
                    }
                    continue;
                }
                let payload: Arc<str> = payload.into();
                let json = json;
                if json
                    .as_ref()
                    .is_some_and(is_machine_wide_broadcast_notification)
                {
                    for client in clients.values() {
                        let _ = client.tx.try_send(ClientOutbound::Acp(payload.clone()));
                    }
                    trace!("Broadcast machine-wide notification to all clients");
                    continue;
                }
                if let Some(target) = json.as_ref().and_then(extract_target_client_id) {
                    if let Some(client) = clients.get(&target) {
                        match json.as_ref().and_then(extract_child_session_event) {
                            Some(ChildSessionEvent::Spawned(child_sid)) => {
                                if let Some(parent) = json.as_ref().and_then(extract_session_id) {
                                    child_sessions
                                        .entry(parent)
                                        .or_default()
                                        .insert(child_sid.clone());
                                }
                                debug!(
                                    client_id = target.0, child_session_id = % child_sid,
                                    "Registered child route from replayed SubagentSpawned"
                                );
                                session_subscribers
                                    .entry(child_sid)
                                    .or_default()
                                    .insert(target);
                            }
                            Some(ChildSessionEvent::Finished(child_sid)) => {
                                let emptied =
                                    session_subscribers.get_mut(&child_sid).is_some_and(|subs| {
                                        subs.remove(&target);
                                        subs.is_empty()
                                    });
                                if emptied {
                                    prune_child_route(
                                        &child_sid,
                                        &mut session_subscribers,
                                        &mut session_driver,
                                        &mut child_sessions,
                                    );
                                }
                            }
                            None => {}
                        }
                        let replay_seq = json
                            .as_ref()
                            .and_then(extract_session_id)
                            .zip(json.as_ref().and_then(event_seq_of));
                        match client.tx.try_send(ClientOutbound::Acp(payload)) {
                            Ok(true) => {
                                if let Some((sid, seq)) = replay_seq {
                                    let entry =
                                        load_replay_max_seq.entry((target, sid)).or_insert(0);
                                    *entry = (*entry).max(seq);
                                }
                                trace!(
                                    client_id = target.0,
                                    "Unicast replay notification to loading client"
                                );
                            }
                            Ok(false) => {
                                warn!(
                                    client_id = target.0,
                                    "Replay notification dropped: loading client channel full (not counted toward flush cutoff)"
                                );
                            }
                            Err(e) => {
                                warn!(
                                    client_id = target.0, error = % e,
                                    "Failed to unicast replay notification to loading client (channel closed)"
                                );
                            }
                        }
                    } else {
                        if let Some(ChildSessionEvent::Finished(child_sid)) =
                            json.as_ref().and_then(extract_child_session_event)
                            && session_subscribers
                                .get(&child_sid)
                                .is_none_or(|subs| subs.is_empty())
                        {
                            prune_child_route(
                                &child_sid,
                                &mut session_subscribers,
                                &mut session_driver,
                                &mut child_sessions,
                            );
                        }
                        if orphan_replay_warned.insert(target) {
                            warn!(
                                client_id = target.0,
                                "Dropping targeted replay notification: loading client disconnected mid-replay (rest of burst logged at trace)"
                            );
                        } else {
                            trace!(
                                client_id = target.0,
                                "Dropping targeted replay notification: loading client disconnected mid-replay"
                            );
                        }
                    }
                    continue;
                }
                let session_id = json.as_ref().and_then(extract_session_id).or_else(|| {
                    json.as_ref()
                        .and_then(extract_session_id_from_prompt_complete)
                });
                if let Some(ref sid) = session_id
                    && let Some(tcid) = json
                        .as_ref()
                        .and_then(extract_interaction_resolved_tool_call_id)
                    && let Some(map) = interaction_requests.get_mut(sid.as_str())
                {
                    map.remove(&tcid);
                    if map.is_empty() {
                        interaction_requests.remove(sid.as_str());
                    }
                }
                let is_reverse_request = json
                    .as_ref()
                    .is_some_and(|j| j.get("id").is_some() && j.get("method").is_some());
                let is_inject_prompt = json.as_ref().is_some_and(is_scheduled_task_inject_prompt);
                let is_interaction =
                    is_reverse_request && json.as_ref().is_some_and(is_interaction_request);
                if is_interaction
                    && let Some(ref sid) = session_id
                    && let Some(tcid) = json.as_ref().and_then(extract_interaction_tool_call_id)
                {
                    interaction_requests
                        .entry(sid.clone())
                        .or_default()
                        .insert(tcid, payload.clone());
                }
                if let Some(ref sid) = session_id
                    && session_subscribers.contains_key(sid.as_str())
                {
                    let child_event = json.as_ref().and_then(extract_child_session_event);
                    let event_seq = json.as_ref().and_then(event_seq_of);
                    if (is_reverse_request && !is_interaction) || is_inject_prompt {
                        if let Some(&driver_id) = session_driver.get(sid.as_str()) {
                            if let Some(client) = clients.get(&driver_id) {
                                if let Err(e) =
                                    client.tx.try_send(ClientOutbound::Acp(payload.clone()))
                                {
                                    warn!(
                                        client_id = driver_id.0, session_id = sid.as_str(),
                                        is_inject = is_inject_prompt, error = % e,
                                        "Failed to route driver-only message (channel closed)"
                                    );
                                } else {
                                    trace!(
                                        client_id = driver_id.0,
                                        session_id = sid.as_str(),
                                        is_inject = is_inject_prompt,
                                        "Routed driver-only message to driver"
                                    );
                                }
                            } else {
                                trace!(
                                    session_id = sid.as_str(),
                                    is_inject = is_inject_prompt,
                                    "Dropping driver-only message: no live driver"
                                );
                            }
                        } else {
                            trace!(
                                session_id = sid.as_str(),
                                is_inject = is_inject_prompt,
                                "Dropping driver-only message: session has no driver"
                            );
                        }
                    } else if let Some(subs) = session_subscribers.get(sid.as_str()) {
                        for &cid in subs.iter() {
                            if let Some(buf) = load_live_buffer.get_mut(&(cid, sid.clone())) {
                                if buf.len() < MAX_BUFFERED_LIVE_PER_LOAD {
                                    buf.push((payload.clone(), event_seq));
                                    trace!(
                                        client_id = cid.0,
                                        session_id = sid.as_str(),
                                        "Buffered live notification during in-flight load"
                                    );
                                    continue;
                                }
                                warn!(
                                    client_id = cid.0,
                                    session_id = sid.as_str(),
                                    "Live buffer for in-flight load exceeded cap; forwarding live (ordering not guaranteed)"
                                );
                            }
                            if let Some(client) = clients.get(&cid) {
                                if let Err(e) =
                                    client.tx.try_send(ClientOutbound::Acp(payload.clone()))
                                {
                                    warn!(
                                        client_id = cid.0, session_id = sid.as_str(), error = % e,
                                        "Failed to broadcast notification to subscriber (channel closed)"
                                    );
                                } else {
                                    trace!(
                                        client_id = cid.0,
                                        session_id = sid.as_str(),
                                        "Broadcast notification to subscriber"
                                    );
                                }
                            }
                        }
                    }
                    match child_event {
                        Some(ChildSessionEvent::Spawned(child_sid)) => {
                            let parent_subs = session_subscribers
                                .get(sid.as_str())
                                .cloned()
                                .unwrap_or_default();
                            info!(
                                child_session_id = % child_sid, subscriber_count =
                                parent_subs.len(),
                                "Registered child session from SubagentSpawned"
                            );
                            session_subscribers.insert(child_sid.clone(), parent_subs);
                            if let Some(&driver_id) = session_driver.get(sid.as_str()) {
                                session_driver.insert(child_sid.clone(), driver_id);
                            }
                            child_sessions
                                .entry(sid.clone())
                                .or_default()
                                .insert(child_sid);
                        }
                        Some(ChildSessionEvent::Finished(child_sid)) => {
                            debug!(
                                child_session_id = % child_sid,
                                "Deregistered child session from SubagentFinished"
                            );
                            prune_child_route(
                                &child_sid,
                                &mut session_subscribers,
                                &mut session_driver,
                                &mut child_sessions,
                            );
                        }
                        None => {}
                    }
                    continue;
                }
                let is_notification = json.as_ref().is_some_and(|j| j.get("id").is_none());
                let is_relay_session_notification = is_notification
                    && session_id
                        .as_ref()
                        .is_some_and(|s| !session_subscribers.contains_key(s.as_str()));
                if !is_notification {
                    trace!("Dropping non-routable response (likely relay-originated)");
                } else if is_relay_session_notification {
                    if let Some(ChildSessionEvent::Finished(child_sid)) =
                        json.as_ref().and_then(extract_child_session_event)
                        && session_subscribers
                            .get(&child_sid)
                            .is_none_or(|subs| subs.is_empty())
                    {
                        prune_child_route(
                            &child_sid,
                            &mut session_subscribers,
                            &mut session_driver,
                            &mut child_sessions,
                        );
                    }
                    trace!(
                        "Dropping notification for relay-owned session (already delivered via WS)"
                    );
                } else if let Some(client_id) = last_active_client
                    && let Some(client) = clients.get(&client_id)
                {
                    debug!(
                        client_id = client_id.0,
                        "Using fallback routing to last active client"
                    );
                    if let Err(e) = client.tx.try_send(ClientOutbound::Acp(payload)) {
                        warn!(
                            client_id = client_id.0, error = % e,
                            "Failed to send notification via fallback routing (channel closed)"
                        );
                    }
                } else {
                    debug!("No client available for notification routing, message dropped");
                }
            }
        }
    }
    finalize_workspace_on_shutdown(control_state.clone()).await;
    finalize_cpu_profile_on_shutdown(control_state).await;
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}
fn spawn_client_handler(
    client_id: ClientId,
    stream: LeaderStream,
    server_rx: AsyncReceiver<ClientOutbound>,
    event_tx: AsyncSender<ServerEvent>,
    cancel: CancellationToken,
    ready_rx: watch::Receiver<bool>,
    control_state: LeaderServerControlState,
) {
    tokio::spawn(async move {
        let result = run_client_session(
            client_id,
            stream,
            server_rx,
            event_tx.clone(),
            cancel,
            ready_rx,
            control_state,
        )
        .await;
        if let Err(e) = &result {
            debug!(client_id = client_id.0, error = % e, "Client session ended");
        }
        let _ = event_tx.send(ServerEvent::Disconnected(client_id)).await;
    });
}
async fn run_client_session(
    client_id: ClientId,
    stream: LeaderStream,
    server_rx: AsyncReceiver<ClientOutbound>,
    event_tx: AsyncSender<ServerEvent>,
    cancel: CancellationToken,
    mut ready_rx: watch::Receiver<bool>,
    control_state: LeaderServerControlState,
) -> Result<(), ProtocolError> {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let msg: ClientMessage =
        match tokio::time::timeout(REGISTRATION_TIMEOUT, read_message(&mut reader)).await {
            Ok(Ok(msg)) => msg,
            Ok(Err(e)) => {
                warn!(client_id = client_id.0, error = % e, "Registration failed");
                return Err(e);
            }
            Err(_) => {
                warn!(
                    client_id = client_id.0,
                    "Registration timeout - client did not register within {:?}",
                    REGISTRATION_TIMEOUT
                );
                let _ = write_message(
                    &mut writer,
                    &ServerMessage::Error {
                        code: 3,
                        message: "Registration timeout".into(),
                    },
                )
                .await;
                return Ok(());
            }
        };
    let (client_type, mode, capabilities, was_ready_at_registration) = match msg {
        ClientMessage::Register {
            client_type,
            mode,
            capabilities,
        } => {
            let ready = *ready_rx.borrow();
            write_message(
                &mut writer,
                &ServerMessage::Registered {
                    client_id: client_id.0,
                    ready,
                    leader_protocol_version: Some(LEADER_PROTOCOL_VERSION),
                    leader_binary_version: Some(
                        control_state.metadata.leader_binary_version.clone(),
                    ),
                    leader_capabilities: Some(control_state.leader_capabilities()),
                },
            )
            .await?;
            (client_type, mode, capabilities, ready)
        }
        _ => {
            write_message(
                &mut writer,
                &ServerMessage::Error {
                    code: 1,
                    message: "Expected Register message".into(),
                },
            )
            .await?;
            return Ok(());
        }
    };
    if !was_ready_at_registration {
        debug!(
            client_id = client_id.0,
            "Client registered before leader ready; waiting for readiness"
        );
        while !*ready_rx.borrow() {
            tokio::select! {
                biased; _ = cancel.cancelled() => { drain_client_outbound_on_cancel(&
                server_rx, & mut writer). await; return Ok(()); } result = ready_rx
                .changed() => { if result.is_err() { return Ok(()); } }
            }
        }
        write_message(&mut writer, &ServerMessage::LeaderReady).await?;
        debug!(
            client_id = client_id.0,
            "Leader ready; sent LeaderReady to client"
        );
    }
    let _ = event_tx
        .send(ServerEvent::Registered(
            client_id,
            mode,
            capabilities.clone(),
            client_type.clone(),
        ))
        .await;
    info!(
        client_id = client_id.0, client_type = % client_type, ? mode, yolo_mode =
        capabilities.yolo_mode, client_version = ? capabilities.client_version,
        "Client registered"
    );
    loop {
        tokio::select! {
            biased; _ = cancel.cancelled() => { drain_client_outbound_on_cancel(&
            server_rx, & mut writer). await; break; } Ok(msg) = server_rx.recv() => { if
            write_outbound(& mut writer, & msg). await .is_err() { break; } } msg_result
            = read_message::< _, ClientMessage > (& mut reader) => { match
            handle_client_inbound_message(msg_result, client_id, & event_tx, & mut
            writer,). await ? { ClientSessionAction::Continue => {}
            ClientSessionAction::Break => break, } }
        }
    }
    Ok(())
}
enum ClientSessionAction {
    Continue,
    Break,
}
async fn drain_client_outbound_on_cancel<W>(
    server_rx: &AsyncReceiver<ClientOutbound>,
    writer: &mut W,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    for _ in 0..10 {
        if !server_rx.is_empty() {
            break;
        }
        tokio::task::yield_now().await;
    }
    while let Ok(Some(msg)) = server_rx.try_recv() {
        if write_outbound(writer, &msg).await.is_err() {
            break;
        }
    }
}
async fn handle_client_inbound_message<W>(
    msg_result: Result<ClientMessage, ProtocolError>,
    client_id: ClientId,
    event_tx: &AsyncSender<ServerEvent>,
    writer: &mut W,
) -> Result<ClientSessionAction, ProtocolError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    match msg_result {
        Ok(msg @ (ClientMessage::Acp { .. } | ClientMessage::Control { .. })) => {
            let _ = event_tx.send(ServerEvent::Message(client_id, msg)).await;
            Ok(ClientSessionAction::Continue)
        }
        Ok(ClientMessage::Ping) => {
            write_message(writer, &ServerMessage::Pong).await?;
            Ok(ClientSessionAction::Continue)
        }
        Ok(ClientMessage::Disconnect) | Err(ProtocolError::ConnectionClosed) => {
            info!(client_id = client_id.0, "Client disconnected");
            Ok(ClientSessionAction::Break)
        }
        Ok(ClientMessage::Register { .. }) => {
            write_message(
                writer,
                &ServerMessage::Error {
                    code: 2,
                    message: "Already registered".into(),
                },
            )
            .await?;
            Ok(ClientSessionAction::Continue)
        }
        Err(e) => {
            warn!(client_id = client_id.0, error = % e, "Protocol error");
            Ok(ClientSessionAction::Break)
        }
    }
}
/// Broadcast a planned shutdown to all connected clients.
///
/// Sends `ShuttingDown` (advance notice with reason and `delay_ms: 0`)
/// followed immediately by `Shutdown`. Both messages are sent before the
/// server exits, so clients that process the channel quickly will see both.
///
/// `delay_ms` is set to 0 because the server sends `Shutdown` immediately
/// after `ShuttingDown` — there is no actual grace period. The cancel token
/// propagates to client session handlers simultaneously, so a sleep between
/// the two messages would allow session writers to exit before `Shutdown`
/// is delivered. Clients should treat `ShuttingDown` as a signal that
/// `Shutdown` is imminent and pre-arm their reconnection handlers.
async fn broadcast_shutdown(
    clients: &HashMap<ClientId, ClientState>,
    reason: super::protocol::ShutdownReason,
) {
    for client in clients.values() {
        let _ = client
            .tx
            .send(
                ServerMessage::ShuttingDown {
                    reason: reason.clone(),
                    delay_ms: 0,
                }
                .into(),
            )
            .await;
        let _ = client.tx.send(ServerMessage::Shutdown.into()).await;
    }
}
pub struct ServerHandle {
    pub cancel: CancellationToken,
    /// Receive ACP messages from clients (server routes them here)
    pub acp_rx: mpsc::UnboundedReceiver<String>,
    /// Send ACP responses back (server routes to correct client based on request ID)
    pub response_tx: mpsc::UnboundedSender<String>,
    /// Atomic counter tracking the number of connected clients
    pub client_count: Arc<AtomicUsize>,
    /// Atomic flag: `true` while the agent has pending (in-flight) requests
    pub agent_busy: Arc<AtomicBool>,
    /// Signal the IPC server that the leader is fully ready (auth + prefetch complete).
    ///
    /// Send `true` once the leader has finished initializing. Until then, ACP requests
    /// receive a `leader_starting` error and ACP notifications are dropped.
    ///
    /// `spawn_leader_server` sends `true` immediately so that callers that do not need
    /// staged startup (e.g. tests, in-process use) get a fully-ready server out of the box.
    /// Production leader startup (`run_leader`) holds this back until auth + prefetch succeed.
    pub ready_tx: watch::Sender<bool>,
    /// Set the shutdown reason before cancelling so clients receive the correct `ShuttingDown`
    /// reason. The default value is [`ShutdownReason::Manual`]; send
    /// [`ShutdownReason::AutoUpdate`] before cancelling for auto-update shutdowns.
    pub shutdown_tx: watch::Sender<super::protocol::ShutdownReason>,
    /// Observe relay demand: flips to `true` when the first headless client
    /// registers (see `relay_demand_tx` on [`run_leader_server`]).
    pub relay_demand_rx: watch::Receiver<bool>,
    /// Leader-local control metadata and CPU profiling state, exposed for tests.
    pub control_state: LeaderServerControlState,
}
fn default_test_control_state(socket_path: &Path) -> LeaderServerControlState {
    LeaderServerControlState::new(LeaderServerMetadata {
        pid: std::process::id(),
        socket_path: socket_path.to_path_buf(),
        lock_path: socket_path.with_extension("lock"),
        ws_url_suffix: String::new(),
        leader_binary_version: env!("CARGO_PKG_VERSION").to_string(),
    })
}
pub async fn spawn_leader_server(socket_path: PathBuf) -> Result<ServerHandle, ServerError> {
    let (acp_tx, acp_rx) = mpsc::unbounded_channel();
    let (response_tx, response_rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let client_count = Arc::new(AtomicUsize::new(0));
    let agent_busy = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = watch::channel(true);
    let (shutdown_tx, _shutdown_reason_rx) =
        watch::channel(super::protocol::ShutdownReason::Manual);
    let (relay_demand_tx, relay_demand_rx) = watch::channel(false);
    let control_state = default_test_control_state(&socket_path);
    let cancel_clone = cancel.clone();
    let socket_path_clone = socket_path.clone();
    let client_count_clone = client_count.clone();
    let agent_busy_clone = agent_busy.clone();
    let control_state_for_server = control_state.clone();
    let shutdown_tx_for_server = shutdown_tx.clone();
    tokio::spawn(async move {
        if let Err(e) = run_leader_server(
            socket_path_clone,
            acp_tx,
            response_rx,
            cancel_clone,
            false,
            client_count_clone,
            agent_busy_clone,
            AgentActivity::default(),
            ready_rx,
            relay_demand_tx,
            shutdown_tx_for_server,
            None,
            control_state_for_server,
        )
        .await
        {
            error!(error = % e, "Leader server error");
        }
    });
    Ok(ServerHandle {
        cancel,
        acp_rx,
        response_tx,
        client_count,
        agent_busy,
        ready_tx,
        shutdown_tx,
        relay_demand_rx,
        control_state,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;
    /// Parse a raw payload for the parse-once helper APIs. Panics on invalid
    /// JSON — the routing loop parses once up front, and non-JSON payloads
    /// never reach the helpers (they forward/drop verbatim).
    fn pv(payload: &str) -> serde_json::Value {
        serde_json::from_str(payload).expect("test payload must be valid JSON")
    }
    /// The relaunch drain must wait on the agent-derived activity signal —
    /// not just the IPC `agent_busy` flag, which relay-driven turns never set
    /// — and must flush registered session actors before cancelling.
    #[tokio::test]
    async fn relaunch_drain_waits_for_agent_activity_and_flushes_sessions() {
        let (shutdown_tx, _shutdown_rx) =
            watch::channel(super::super::protocol::ShutdownReason::Manual);
        let cancel = CancellationToken::new();
        let agent_busy = Arc::new(AtomicBool::new(false));
        let activity = AgentActivity::default();
        let (mut cmd_rx, prompt_id, _pending) = activity.register_for_test("s1");
        *prompt_id.lock().unwrap() = Some("prompt-1".to_string());
        let cancel_for_actor = cancel.clone();
        let actor = tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                if matches!(cmd, crate::session::SessionCommand::Shutdown) {
                    assert!(
                        !cancel_for_actor.is_cancelled(),
                        "flush must run before the leader cancels"
                    );
                    return;
                }
            }
        });
        spawn_relaunch_drain(shutdown_tx, cancel.clone(), agent_busy, activity);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !cancel.is_cancelled(),
            "drain must not cancel while a relay-driven turn is running"
        );
        *prompt_id.lock().unwrap() = None;
        tokio::time::timeout(Duration::from_secs(5), cancel.cancelled())
            .await
            .expect("drain should cancel once the agent goes idle");
        actor.await.expect("session actor should get Shutdown");
    }
    /// `ServerMessageRef::Acp` (the borrowed serialize-only mirror the client
    /// writer uses for shared payloads) must stay byte-identical on the wire
    /// to `ServerMessage::Acp`, or clients would fail to decode ACP frames.
    #[test]
    fn server_message_ref_is_wire_identical() {
        let payload = r#"{"jsonrpc":"2.0","method":"session/update","params":{"x":1}}"#;
        let owned = serde_json::to_vec(&ServerMessage::Acp {
            payload: payload.to_string(),
        })
        .unwrap();
        let borrowed = serde_json::to_vec(&ServerMessageRef::Acp { payload }).unwrap();
        assert_eq!(owned, borrowed);
        let decoded: ServerMessage = serde_json::from_slice(&borrowed).unwrap();
        match decoded {
            ServerMessage::Acp { payload: p } => assert_eq!(p, payload),
            other => panic!("expected Acp, got {other:?}"),
        }
    }
    /// An UNMUTATED payload forwards to the agent byte-for-byte: parsing for
    /// classification must never normalize key order or whitespace of
    /// pass-through traffic.
    #[test]
    fn outbound_payload_verbatim_when_unmutated() {
        let original = r#"{ "b" : 1,    "a": 2 }"#.to_string();
        let json = pv(&original);
        let out = select_outbound_payload(Some(&json), false, original.clone());
        assert_eq!(
            out, original,
            "unmutated payloads must forward verbatim (exact bytes, not re-serialized)"
        );
    }
    /// A MUTATED payload is re-serialized from the injected/rewritten `Value`
    /// (semantically equal, but no longer the original odd formatting).
    #[test]
    fn outbound_payload_reserialized_when_mutated() {
        let original = r#"{ "b" : 1,    "a": 2 }"#.to_string();
        let json = pv(&original);
        let out = select_outbound_payload(Some(&json), true, original.clone());
        assert_ne!(
            out, original,
            "mutated payloads must be re-serialized from the Value, not the stale original"
        );
        assert_eq!(
            pv(&out),
            json,
            "the re-serialized payload must be semantically identical to the mutated Value"
        );
    }
    /// A non-JSON payload (`json = None`) is never parsed or re-serialized —
    /// it passes through untouched, matching the old per-helper parse-failure
    /// behavior.
    #[test]
    fn outbound_payload_non_json_passthrough() {
        let original = "not json".to_string();
        let out = select_outbound_payload(None, false, original.clone());
        assert_eq!(
            out, original,
            "non-JSON payloads must pass through verbatim"
        );
    }
    #[test]
    fn decide_relaunch_is_idempotent_and_directional() {
        let temp = TempDir::new().unwrap();
        let sock = temp.path().join("leader.sock");
        let control_state = LeaderServerControlState::new(LeaderServerMetadata {
            pid: std::process::id(),
            socket_path: sock.clone(),
            lock_path: sock.with_extension("lock"),
            ws_url_suffix: String::new(),
            leader_binary_version: "0.1.100".to_string(),
        });
        let relaunching = AtomicBool::new(false);
        assert!(matches!(
            decide_relaunch_for_update(&control_state, "0.1.100".to_string(), &relaunching),
            Ok(ControlPayload::RelaunchDeclined { .. })
        ));
        assert!(!relaunching.load(Ordering::SeqCst));
        assert!(matches!(
            decide_relaunch_for_update(&control_state, "0.1.0".to_string(), &relaunching),
            Ok(ControlPayload::RelaunchDeclined { .. })
        ));
        assert!(matches!(
            decide_relaunch_for_update(&control_state, "unknown".to_string(), &relaunching),
            Ok(ControlPayload::RelaunchDeclined { .. })
        ));
        assert!(!relaunching.load(Ordering::SeqCst));
        assert!(matches!(
            decide_relaunch_for_update(&control_state, "0.2.0".to_string(), &relaunching),
            Ok(ControlPayload::Relaunching { .. })
        ));
        assert!(relaunching.load(Ordering::SeqCst));
        assert!(matches!(
            decide_relaunch_for_update(&control_state, "0.3.0".to_string(), &relaunching),
            Ok(ControlPayload::RelaunchDeclined { .. })
        ));
    }
    #[derive(Debug)]
    struct TestAuth;
    impl AuthProvider for TestAuth {
        fn current(&self) -> AuthCredential {
            AuthCredential::bearer("test-token")
        }
    }
    #[tokio::test]
    async fn wait_for_leader_auth_returns_when_already_wired() {
        let ws = WorkspaceControl::new(None);
        ws.auth.send_replace(Some(Arc::new(TestAuth)));
        let cancel = CancellationToken::new();
        let auth = wait_for_leader_auth(&ws, &cancel).await.expect("wired");
        assert!(matches!(auth.current(), AuthCredential::Bearer { .. }));
    }
    #[tokio::test]
    async fn wait_for_leader_auth_resolves_when_wired_late() {
        let ws = Arc::new(WorkspaceControl::new(None));
        let cancel = CancellationToken::new();
        let waiter = {
            let ws = ws.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move { wait_for_leader_auth(&ws, &cancel).await.is_ok() })
        };
        tokio::task::yield_now().await;
        ws.auth.send_replace(Some(Arc::new(TestAuth)));
        assert!(waiter.await.unwrap(), "auth wired late should resolve Ok");
    }
    #[tokio::test]
    async fn workspace_start_errors_when_cancelled_before_auth() {
        let state = default_test_control_state(Path::new("/tmp/grok-ws-auth-test.sock"));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = handle_workspace_start(state, None, "/tmp".to_string(), cancel)
            .await
            .unwrap_err();
        assert!(
            err.message.contains("shutting down"),
            "unexpected error: {}",
            err.message
        );
    }
    async fn setup_test_server(
        temp: &TempDir,
    ) -> (PathBuf, CancellationToken, mpsc::UnboundedReceiver<String>) {
        let sock_path = temp.path().join("test.sock");
        let handle = spawn_leader_server(sock_path.clone()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        (sock_path, handle.cancel, handle.acp_rx)
    }
    async fn setup_test_server_with_client_count(
        temp: &TempDir,
    ) -> (
        PathBuf,
        CancellationToken,
        mpsc::UnboundedReceiver<String>,
        Arc<AtomicUsize>,
    ) {
        let sock_path = temp.path().join("test.sock");
        let handle = spawn_leader_server(sock_path.clone()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        (sock_path, handle.cancel, handle.acp_rx, handle.client_count)
    }
    /// Like `setup_test_server` but uses `no_exit_on_disconnect=true` and
    /// exposes `response_tx` for injecting agent responses.
    async fn setup_persistent_server(
        temp: &TempDir,
    ) -> (PathBuf, CancellationToken, mpsc::UnboundedSender<String>) {
        let (sock_path, cancel, response_tx, _acp_rx) =
            setup_persistent_server_with_agent(temp).await;
        (sock_path, cancel, response_tx)
    }
    /// Like `setup_persistent_server` but also returns the agent-side receiver
    /// (`acp_rx`) so a test can observe forwarded requests — e.g. to read a
    /// `session/load`'s namespaced id and echo a matching load response, which
    /// is required to complete a load now that live broadcasts to a loading
    /// client are buffered until its load response (see `complete_load`).
    async fn setup_persistent_server_with_agent(
        temp: &TempDir,
    ) -> (
        PathBuf,
        CancellationToken,
        mpsc::UnboundedSender<String>,
        mpsc::UnboundedReceiver<String>,
    ) {
        let sock_path = temp.path().join("test.sock");
        let (acp_tx, acp_rx) = mpsc::unbounded_channel();
        let (response_tx, response_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let control_state = default_test_control_state(&sock_path);
        let sock_clone = sock_path.clone();
        let cancel_clone = cancel.clone();
        let (_ready_tx, ready_rx) = watch::channel(true);
        let (shutdown_tx, _shutdown_rx) =
            watch::channel(super::super::protocol::ShutdownReason::Manual);
        tokio::spawn(async move {
            let _ = run_leader_server(
                sock_clone,
                acp_tx,
                response_rx,
                cancel_clone,
                true,
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicBool::new(false)),
                AgentActivity::default(),
                ready_rx,
                watch::channel(false).0,
                shutdown_tx,
                None,
                control_state,
            )
            .await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        (sock_path, cancel, response_tx, acp_rx)
    }
    /// Complete an in-flight `session/load` in a test: read the forwarded load
    /// request from the agent channel to learn its leader-assigned namespaced
    /// id, then echo a `LoadSessionResponse` with that id. This routes the
    /// response back to the loading client AND flushes any live notifications
    /// the leader buffered during the load window (live-before-replay guard).
    async fn complete_load(
        acp_rx: &mut mpsc::UnboundedReceiver<String>,
        response_tx: &mpsc::UnboundedSender<String>,
    ) {
        loop {
            let forwarded = tokio::time::timeout(Duration::from_secs(1), acp_rx.recv())
                .await
                .expect("timed out waiting for forwarded session/load")
                .expect("agent channel closed");
            let json: serde_json::Value = serde_json::from_str(&forwarded).unwrap();
            if json.get("method").and_then(|m| m.as_str()) == Some("session/load") {
                let id = json.get("id").cloned().unwrap();
                let response = serde_json::json!(
                    { "jsonrpc" : "2.0", "id" : id, "result" : { "models" : [] }, }
                );
                response_tx.send(response.to_string()).unwrap();
                return;
            }
        }
    }
    /// Helper to connect and register a client, returning the split stream.
    async fn connect_and_register(
        sock_path: &std::path::Path,
        client_type: &str,
    ) -> (
        tokio::io::ReadHalf<LeaderStream>,
        tokio::io::WriteHalf<LeaderStream>,
    ) {
        connect_and_register_with_mode(sock_path, client_type, ClientMode::Stdio).await
    }
    /// Like [`connect_and_register`] but with an explicit [`ClientMode`], for
    /// tests that exercise mode-dependent server behavior (relay demand).
    async fn connect_and_register_with_mode(
        sock_path: &std::path::Path,
        client_type: &str,
        mode: ClientMode,
    ) -> (
        tokio::io::ReadHalf<LeaderStream>,
        tokio::io::WriteHalf<LeaderStream>,
    ) {
        let stream = LeaderStream::connect(sock_path).await.unwrap();
        let (mut reader, mut writer) = tokio::io::split(stream);
        write_message(
            &mut writer,
            &ClientMessage::Register {
                client_type: client_type.into(),
                mode,
                capabilities: ClientCapabilities::default(),
            },
        )
        .await
        .unwrap();
        let _: ServerMessage = read_message(&mut reader).await.unwrap();
        (reader, writer)
    }
    /// Relay demand gate (relay-on-demand): Stdio registrations must NOT
    /// signal relay demand — a leader serving only interactive clients (TUI
    /// dashboard, IDE) keeps the grok.com relay off. The first Headless
    /// registration (devbox / `grok agent headless` flow) flips the watch so
    /// `run_leader` starts the deferred relay connection.
    #[tokio::test]
    async fn relay_demand_signals_only_on_headless_registration() {
        let temp = TempDir::new().unwrap();
        let sock_path = temp.path().join("relay-demand.sock");
        let handle = spawn_leader_server(sock_path.clone()).await.unwrap();
        let mut relay_demand_rx = handle.relay_demand_rx.clone();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _stdio =
            connect_and_register_with_mode(&sock_path, "grok-tui", ClientMode::Stdio).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !*relay_demand_rx.borrow(),
            "stdio registration must not signal relay demand"
        );
        let _headless =
            connect_and_register_with_mode(&sock_path, "grok-headless", ClientMode::Headless).await;
        tokio::time::timeout(Duration::from_secs(5), relay_demand_rx.wait_for(|d| *d))
            .await
            .expect("relay demand must flip after headless registration")
            .expect("relay demand channel must stay open");
        handle.cancel.cancel();
    }
    #[tokio::test]
    async fn client_registration_flow() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, _acp_rx) = setup_test_server(&temp).await;
        let stream = LeaderStream::connect(&sock_path).await.unwrap();
        let (mut reader, mut writer) = tokio::io::split(stream);
        write_message(
            &mut writer,
            &ClientMessage::Register {
                client_type: "test".into(),
                mode: ClientMode::Stdio,
                capabilities: ClientCapabilities::default(),
            },
        )
        .await
        .unwrap();
        let response: ServerMessage = read_message(&mut reader).await.unwrap();
        match response {
            ServerMessage::Registered {
                client_id,
                ready,
                leader_protocol_version,
                leader_binary_version,
                leader_capabilities,
            } => {
                assert!(ready);
                assert!(client_id > 0);
                assert_eq!(leader_protocol_version, Some(LEADER_PROTOCOL_VERSION));
                assert_eq!(
                    leader_binary_version.as_deref(),
                    Some(env!("CARGO_PKG_VERSION"))
                );
                let capabilities = leader_capabilities.expect("leader capabilities metadata");
                assert!(capabilities.control_v1);
                assert_eq!(
                    capabilities.runtime_cpu_profile,
                    CpuProfileManager::new().runtime_cpu_profile()
                );
            }
            _ => panic!("Expected Registered response"),
        }
        cancel.cancel();
    }
    #[tokio::test]
    async fn control_requests_bypass_acp_routing() {
        let temp = TempDir::new().unwrap();
        let sock_path = temp.path().join("test.sock");
        let mut handle = spawn_leader_server(sock_path.clone()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stream = LeaderStream::connect(&sock_path).await.unwrap();
        let (mut reader, mut writer) = tokio::io::split(stream);
        write_message(
            &mut writer,
            &ClientMessage::Register {
                client_type: "test".into(),
                mode: ClientMode::Stdio,
                capabilities: ClientCapabilities::default(),
            },
        )
        .await
        .unwrap();
        let _: ServerMessage = read_message(&mut reader).await.unwrap();
        write_message(
            &mut writer,
            &ClientMessage::Control {
                request_id: "status-1".into(),
                command: ControlCommand::CpuProfileStatus,
            },
        )
        .await
        .unwrap();
        let response: ServerMessage = read_message(&mut reader).await.unwrap();
        assert!(
            matches!(response, ServerMessage::ControlResult { request_id, result :
            Ok(ControlPayload::CpuProfileStatus { active : false, stopping : false,
            started_at : None, svg_path : None, frequency_hz : None, }), }
if request_id
            == "status-1")
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), handle.acp_rx.recv())
                .await
                .is_err()
        );
        handle.cancel.cancel();
    }
    #[tokio::test]
    async fn shutdown_waits_for_in_flight_cpu_profile_stop() {
        let temp = TempDir::new().unwrap();
        let sock_path = temp.path().join("test.sock");
        let output_path = temp.path().join("shutdown-runtime-profile.folded");
        let control_state = default_test_control_state(&sock_path);
        let stop_handle = {
            let mut manager = control_state.cpu_profile.lock();
            if !manager.runtime_cpu_profile() {
                return;
            }
            let Ok(_) = manager.start(CpuProfileStartOptions {
                output: Some(output_path.clone()),
                frequency_hz: Some(200),
            }) else {
                return;
            };
            manager.take_stop_handle().unwrap()
        };
        let control_state_for_shutdown = control_state.clone();
        let shutdown_wait = tokio::spawn(async move {
            finalize_cpu_profile_on_shutdown(control_state_for_shutdown).await;
        });
        let control_state_for_stop = control_state.clone();
        let in_flight_stop = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let result = tokio::task::spawn_blocking(move || stop_handle.finish())
                .await
                .unwrap()
                .unwrap();
            control_state_for_stop.cpu_profile.lock().complete_stop();
            result
        });
        tokio::time::timeout(Duration::from_secs(5), shutdown_wait)
            .await
            .expect("shutdown wait should complete")
            .unwrap();
        let stop_result = tokio::time::timeout(Duration::from_secs(5), in_flight_stop)
            .await
            .expect("in-flight stop should complete")
            .unwrap();
        assert_eq!(stop_result.svg_path, output_path);
        assert!(output_path.exists());
        assert!(matches!(
            control_state.cpu_profile.lock().status(),
            CpuProfileStatus::Inactive
        ));
    }
    #[tokio::test]
    async fn runtime_profile_reports_unsupported_build_end_to_end() {
        let temp = TempDir::new().unwrap();
        let sock_path = temp.path().join("leader-unsupported.sock");
        let handle = spawn_leader_server(sock_path.clone()).await.unwrap();
        {
            let mut manager = handle.control_state.cpu_profile.lock();
            manager.force_unsupported_for_test();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let client = super::super::client::LeaderClient::connect(
            sock_path,
            "client",
            ClientMode::Stdio,
            ClientCapabilities::default(),
        )
        .await
        .unwrap();
        let runtime_cpu_profile = client
            .registration()
            .leader_capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.runtime_cpu_profile);
        assert!(
            !runtime_cpu_profile,
            "unsupported stub server must report runtime_cpu_profile=false"
        );
        let status = client
            .send_control(ControlCommand::CpuProfileStatus)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            status,
            ControlPayload::CpuProfileStatus {
                active: false,
                stopping: false,
                started_at: None,
                svg_path: None,
                frequency_hz: None,
            }
        ));
        let start_err = client
            .send_control(ControlCommand::StartCpuProfile {
                output: None,
                frequency_hz: None,
            })
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(
            start_err.code,
            crate::cpu_profile::ControlErrorCode::RuntimeProfilingUnsupported
        );
        let stop_err = client
            .send_control(ControlCommand::StopCpuProfile)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(
            stop_err.code,
            crate::cpu_profile::ControlErrorCode::ProfileNotActive
        );
        client.cancel();
        handle.cancel.cancel();
    }
    #[tokio::test]
    async fn ping_pong() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, _acp_rx) = setup_test_server(&temp).await;
        let stream = LeaderStream::connect(&sock_path).await.unwrap();
        let (mut reader, mut writer) = tokio::io::split(stream);
        write_message(
            &mut writer,
            &ClientMessage::Register {
                client_type: "test".into(),
                mode: ClientMode::Stdio,
                capabilities: ClientCapabilities::default(),
            },
        )
        .await
        .unwrap();
        let _: ServerMessage = read_message(&mut reader).await.unwrap();
        write_message(&mut writer, &ClientMessage::Ping)
            .await
            .unwrap();
        let response: ServerMessage = read_message(&mut reader).await.unwrap();
        assert!(matches!(response, ServerMessage::Pong));
        cancel.cancel();
    }
    #[tokio::test]
    async fn acp_message_forwarding() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, mut acp_rx) = setup_test_server(&temp).await;
        let stream = LeaderStream::connect(&sock_path).await.unwrap();
        let (mut reader, mut writer) = tokio::io::split(stream);
        write_message(
            &mut writer,
            &ClientMessage::Register {
                client_type: "test".into(),
                mode: ClientMode::Stdio,
                capabilities: ClientCapabilities::default(),
            },
        )
        .await
        .unwrap();
        let _: ServerMessage = read_message(&mut reader).await.unwrap();
        let payload = r#"{"jsonrpc":"2.0","method":"test"}"#;
        write_message(
            &mut writer,
            &ClientMessage::Acp {
                payload: payload.into(),
            },
        )
        .await
        .unwrap();
        let received = acp_rx.recv().await.unwrap();
        assert_eq!(received, payload);
        cancel.cancel();
    }
    #[tokio::test]
    async fn initialize_gets_client_identifier_injected() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, mut acp_rx) = setup_test_server(&temp).await;
        let stream = LeaderStream::connect(&sock_path).await.unwrap();
        let (mut reader, mut writer) = tokio::io::split(stream);
        write_message(
            &mut writer,
            &ClientMessage::Register {
                client_type: "grok-tui".into(),
                mode: ClientMode::Stdio,
                capabilities: ClientCapabilities::default(),
            },
        )
        .await
        .unwrap();
        let _: ServerMessage = read_message(&mut reader).await.unwrap();
        let payload =
            r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"protocolVersion":"0.1"}}"#;
        write_message(
            &mut writer,
            &ClientMessage::Acp {
                payload: payload.into(),
            },
        )
        .await
        .unwrap();
        let received = acp_rx.recv().await.unwrap();
        let json: serde_json::Value = serde_json::from_str(&received).unwrap();
        assert_eq!(
            json["params"]["_meta"]["clientIdentifier"], "grok-tui",
            "Leader should inject clientIdentifier from IPC registration"
        );
        assert_eq!(json["method"], "initialize");
        cancel.cancel();
    }
    #[tokio::test]
    async fn initialize_preserves_existing_client_identifier() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, mut acp_rx) = setup_test_server(&temp).await;
        let stream = LeaderStream::connect(&sock_path).await.unwrap();
        let (mut reader, mut writer) = tokio::io::split(stream);
        write_message(
            &mut writer,
            &ClientMessage::Register {
                client_type: "grok-tui".into(),
                mode: ClientMode::Stdio,
                capabilities: ClientCapabilities::default(),
            },
        )
        .await
        .unwrap();
        let _: ServerMessage = read_message(&mut reader).await.unwrap();
        let payload = r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"protocolVersion":"0.1","_meta":{"clientIdentifier":"grok-web"}}}"#;
        write_message(
            &mut writer,
            &ClientMessage::Acp {
                payload: payload.into(),
            },
        )
        .await
        .unwrap();
        let received = acp_rx.recv().await.unwrap();
        let json: serde_json::Value = serde_json::from_str(&received).unwrap();
        assert_eq!(
            json["params"]["_meta"]["clientIdentifier"], "grok-web",
            "Leader should not override existing clientIdentifier"
        );
        cancel.cancel();
    }
    #[test]
    fn rewrite_request_id_rewrites_requests() {
        let mut json = pv(r#"{"jsonrpc":"2.0","method":"test","id":42,"params":{}}"#);
        let client_id = ClientId(123);
        let (namespaced_id, original_id) = rewrite_request_id(&mut json, client_id).unwrap();
        assert_eq!(original_id, serde_json::json!(42));
        assert_eq!(namespaced_id, "123|42");
        assert_eq!(json["id"], "123|42");
        assert_eq!(json["method"], "test");
    }
    #[test]
    fn is_session_load_request_detects_only_load() {
        assert!(is_session_load_request(&pv(
            r#"{"jsonrpc":"2.0","id":1,"method":"session/load","params":{"sessionId":"s1","cwd":"/tmp"}}"#
        )));
        assert!(!is_session_load_request(&pv(
            r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{}}"#
        )));
        assert!(!is_session_load_request(&pv(
            r#"{"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"sessionId":"s1"}}"#
        )));
        assert!(!is_session_load_request(&pv(
            r#"{"jsonrpc":"2.0","id":1,"result":{}}"#
        )));
    }
    #[test]
    fn is_scheduled_task_inject_prompt_detects_only_inject() {
        assert!(is_scheduled_task_inject_prompt(&pv(
            r#"{"method":"x.ai/scheduled_task_inject_prompt","params":{"sessionId":"s1","taskId":"t1","prompt":"echo hi"}}"#
        )));
        assert!(is_scheduled_task_inject_prompt(&pv(
            r#"{"method":"_x.ai/scheduled_task_inject_prompt","params":{"method":"x.ai/scheduled_task_inject_prompt","params":{"sessionId":"s1","taskId":"t1","prompt":"echo hi"}}}"#
        )));
        assert!(!is_scheduled_task_inject_prompt(&pv(
            r#"{"method":"x.ai/scheduled_task_fired","params":{"sessionId":"s1"}}"#
        )));
        assert!(!is_scheduled_task_inject_prompt(&pv(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1"}}"#
        )));
    }
    #[test]
    fn is_interaction_request_detects_only_interaction_methods() {
        for m in [
            "session/request_permission",
            "x.ai/ask_user_question",
            "x.ai/exit_plan_mode",
        ] {
            let payload = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{m}","params":{{}}}}"#);
            assert!(
                is_interaction_request(&pv(&payload)),
                "{m} (direct) must be an interaction"
            );
        }
        for m in ["x.ai/ask_user_question", "x.ai/exit_plan_mode"] {
            let payload = format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"_{m}","params":{{"method":"{m}","params":{{}}}}}}"#
            );
            assert!(
                is_interaction_request(&pv(&payload)),
                "wrapped {m} must be an interaction"
            );
        }
        assert!(!is_interaction_request(&pv(
            r#"{"jsonrpc":"2.0","id":1,"method":"fs/read_text_file","params":{}}"#
        )));
        assert!(!is_interaction_request(&pv(
            r#"{"jsonrpc":"2.0","method":"x.ai/sessions/changed","params":{}}"#
        )));
    }
    #[test]
    fn extract_interaction_tool_call_id_handles_direct_and_nested() {
        assert_eq!(
            extract_interaction_tool_call_id(&
            pv(r#"{"id":1,"method":"x.ai/ask_user_question","params":{"sessionId":"s","toolCallId":"tc-q"}}"#))
            .as_deref(), Some("tc-q")
        );
        assert_eq!(
            extract_interaction_tool_call_id(&
            pv(r#"{"id":1,"method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"tc-p"}}}"#))
            .as_deref(), Some("tc-p")
        );
        assert_eq!(
            extract_interaction_tool_call_id(&
            pv(r#"{"id":1,"method":"_x.ai/ask_user_question","params":{"method":"x.ai/ask_user_question","params":{"sessionId":"s","toolCallId":"tc-w"}}}"#))
            .as_deref(), Some("tc-w")
        );
        assert_eq!(
            extract_interaction_tool_call_id(&pv(r#"{"params":{}}"#)),
            None
        );
    }
    #[test]
    fn extract_interaction_resolved_tool_call_id_matches_only_resolved() {
        assert_eq!(
            extract_interaction_resolved_tool_call_id(&
            pv(r#"{"method":"x.ai/session_notification","params":{"sessionId":"s","update":{"sessionUpdate":"interaction_resolved","tool_call_id":"tc-r"}}}"#))
            .as_deref(), Some("tc-r")
        );
        assert_eq!(
            extract_interaction_resolved_tool_call_id(&
            pv(r#"{"method":"_x.ai/session_notification","params":{"method":"x.ai/session_notification","params":{"sessionId":"s","update":{"sessionUpdate":"interaction_resolved","tool_call_id":"tc-rw"}}}}"#))
            .as_deref(), Some("tc-rw")
        );
        assert_eq!(
            extract_interaction_resolved_tool_call_id(&pv(
                r#"{"method":"x.ai/session_notification","params":{"sessionId":"s","update":{"sessionUpdate":"pending_interaction","tool_call_id":"tc-r","kind":"permission"}}}"#
            )),
            None
        );
    }
    #[test]
    fn session_load_request_id_matches_response_id_for_buffer_flush() {
        let mut req = pv(
            r#"{"jsonrpc":"2.0","id":7,"method":"session/load","params":{"sessionId":"sess-x","cwd":"/tmp"}}"#,
        );
        assert!(is_session_load_request(&req));
        assert_eq!(extract_session_id(&req).as_deref(), Some("sess-x"));
        let client = ClientId(3);
        let (stored_ns_id, _orig) = rewrite_request_id(&mut req, client).unwrap();
        assert_eq!(stored_ns_id, "3|7");
        assert_eq!(req["id"], stored_ns_id.as_str());
        let mut response = pv(&format!(
            r#"{{"jsonrpc":"2.0","id":"{stored_ns_id}","result":{{"models":[]}}}}"#
        ));
        let (parsed_client, raw_response_id) = parse_response_id(&mut response).unwrap();
        assert_eq!(parsed_client, client);
        assert_eq!(raw_response_id, stored_ns_id);
        assert_eq!(response["id"], serde_json::json!(7));
    }
    #[test]
    fn live_buffer_holds_during_load_and_flushes_in_order() {
        let client = ClientId(5);
        let sid = "sess-y".to_string();
        let mut pending_load_by_req: HashMap<String, (ClientId, String)> = HashMap::new();
        let mut load_live_buffer: HashMap<(ClientId, String), Vec<BufferedLive>> = HashMap::new();
        pending_load_by_req.insert("5|1".to_string(), (client, sid.clone()));
        load_live_buffer.entry((client, sid.clone())).or_default();
        for p in ["e1", "e2", "e3"] {
            if let Some(buf) = load_live_buffer.get_mut(&(client, sid.clone())) {
                buf.push((Arc::from(p), None));
            }
        }
        assert_eq!(
            load_live_buffer
                .get(&(client, sid.clone()))
                .unwrap()
                .iter()
                .map(|(p, _)| p.as_ref())
                .collect::<Vec<_>>(),
            ["e1", "e2", "e3"]
        );
        let flushed = pending_load_by_req
            .remove("5|1")
            .and_then(|(c, s)| load_live_buffer.remove(&(c, s)))
            .unwrap();
        assert_eq!(
            flushed.iter().map(|(p, _)| p.as_ref()).collect::<Vec<_>>(),
            ["e1", "e2", "e3"]
        );
        assert!(pending_load_by_req.is_empty());
        assert!(load_live_buffer.is_empty());
        pending_load_by_req.insert("5|2".to_string(), (client, sid.clone()));
        load_live_buffer.entry((client, sid.clone())).or_default();
        assert!(pending_load_by_req.remove("9|9").is_none());
        assert!(load_live_buffer.contains_key(&(client, sid.clone())));
        pending_load_by_req.retain(|_, (c, _)| *c != client);
        load_live_buffer.retain(|(c, _), _| *c != client);
        assert!(pending_load_by_req.is_empty());
        assert!(load_live_buffer.is_empty());
    }
    /// An `agent_message_chunk` `session/update` carrying `eventId` at
    /// `params._meta.eventId` (the live-broadcast wire shape).
    fn live_chunk(sid: &str, seq: u64) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"{sid}","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"x"}}}},"_meta":{{"eventId":"{sid}-{seq}"}}}}}}"#
        )
    }
    #[test]
    fn event_seq_of_parses_acp_and_ext_and_handles_missing() {
        let acp = pv(r#"{"params":{"sessionId":"019e-aa","_meta":{"eventId":"019e-aa-42"}}}"#);
        assert_eq!(event_seq_of(&acp), Some(42));
        let ext = pv(
            r#"{"params":{"method":"x.ai/session/update","params":{"sessionId":"019e-aa","_meta":{"eventId":"019e-aa-7"}}}}"#,
        );
        assert_eq!(event_seq_of(&ext), Some(7));
        let none = pv(r#"{"params":{"sessionId":"019e-aa","_meta":{}}}"#);
        assert_eq!(event_seq_of(&none), None);
    }
    /// Regression: on a mid-turn attach, the in-flight turn streams + persists
    /// during the [subscribe -> gate-close] window, so its chunks are BOTH
    /// buffered-live for the loading client AND read back by replay (same
    /// eventId). The post-load flush must drop the buffered copies that replay
    /// already delivered (`event_seq <= replay max`) and forward only the
    /// genuinely-newer tail — so each event reaches the client exactly once.
    #[test]
    fn buffer_flush_drops_replay_overlap_by_event_seq() {
        let client = ClientId(5);
        let sid = "sess-z".to_string();
        let mut load_live_buffer: HashMap<(ClientId, String), Vec<BufferedLive>> = HashMap::new();
        let mut load_replay_max_seq: HashMap<(ClientId, String), u64> = HashMap::new();
        for seq in 7..=21u64 {
            let json = pv(&live_chunk(&sid, seq));
            if let Some(s) = extract_session_id(&json)
                && let Some(n) = event_seq_of(&json)
            {
                let e = load_replay_max_seq.entry((client, s)).or_insert(0);
                *e = (*e).max(n);
            }
        }
        assert_eq!(load_replay_max_seq.get(&(client, sid.clone())), Some(&21));
        let buf = load_live_buffer.entry((client, sid.clone())).or_default();
        for seq in 7..=23u64 {
            let payload = live_chunk(&sid, seq);
            let event_seq = event_seq_of(&pv(&payload));
            buf.push((payload.into(), event_seq));
        }
        let cutoff: Option<u64> = load_replay_max_seq.remove(&(client, sid.clone()));
        let buffered = load_live_buffer.remove(&(client, sid.clone())).unwrap();
        let mut forwarded: Vec<u64> = Vec::new();
        for (_, buffered_seq) in &buffered {
            if let Some(c) = cutoff
                && buffered_seq.is_some_and(|s| s <= c)
            {
                continue;
            }
            if let Some(s) = buffered_seq {
                forwarded.push(*s);
            }
        }
        assert_eq!(
            forwarded,
            vec![22, 23],
            "only the post-replay tail is forwarded (overlap 7..=21 dropped)"
        );
    }
    /// Edge case: a fresh process's very first event has `event_seq == 0`. The
    /// cutoff must be an `Option` (not a `> 0` sentinel), so a genuine max of 0
    /// still drops the buffered-live seq-0 duplicate instead of forwarding it.
    #[test]
    fn buffer_flush_drops_replay_overlap_at_seq_zero() {
        let client = ClientId(5);
        let sid = "sess-0".to_string();
        let mut load_live_buffer: HashMap<(ClientId, String), Vec<BufferedLive>> = HashMap::new();
        let mut load_replay_max_seq: HashMap<(ClientId, String), u64> = HashMap::new();
        let json = pv(&live_chunk(&sid, 0));
        if let Some(s) = extract_session_id(&json)
            && let Some(n) = event_seq_of(&json)
        {
            let e = load_replay_max_seq.entry((client, s)).or_insert(0);
            *e = (*e).max(n);
        }
        assert_eq!(load_replay_max_seq.get(&(client, sid.clone())), Some(&0));
        let buf = load_live_buffer.entry((client, sid.clone())).or_default();
        for seq in [0u64, 1] {
            let payload = live_chunk(&sid, seq);
            let event_seq = event_seq_of(&pv(&payload));
            buf.push((payload.into(), event_seq));
        }
        let cutoff: Option<u64> = load_replay_max_seq.remove(&(client, sid.clone()));
        assert_eq!(
            cutoff,
            Some(0),
            "a genuine cutoff of 0 must be Some(0), not absent"
        );
        let buffered = load_live_buffer.remove(&(client, sid.clone())).unwrap();
        let mut forwarded: Vec<u64> = Vec::new();
        for (_, buffered_seq) in &buffered {
            if let Some(c) = cutoff
                && buffered_seq.is_some_and(|s| s <= c)
            {
                continue;
            }
            if let Some(s) = buffered_seq {
                forwarded.push(*s);
            }
        }
        assert_eq!(
            forwarded,
            vec![1],
            "seq-0 duplicate dropped, seq-1 tail forwarded (Option cutoff, not > 0)"
        );
    }
    #[test]
    fn rewrite_request_id_skips_responses_with_result() {
        let mut json = pv(r#"{"jsonrpc":"2.0","result":{"content":"hello"},"id":42}"#);
        let before = json.clone();
        assert!(rewrite_request_id(&mut json, ClientId(123)).is_none());
        assert_eq!(json, before, "payload unchanged");
    }
    #[test]
    fn rewrite_request_id_skips_responses_with_error() {
        let mut json =
            pv(r#"{"jsonrpc":"2.0","error":{"code":-32600,"message":"Invalid"},"id":5}"#);
        let before = json.clone();
        assert!(rewrite_request_id(&mut json, ClientId(123)).is_none());
        assert_eq!(json, before, "payload unchanged");
    }
    #[test]
    fn rewrite_request_id_handles_notifications() {
        let mut json = pv(r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#);
        let before = json.clone();
        assert!(rewrite_request_id(&mut json, ClientId(123)).is_none());
        assert_eq!(json, before, "payload unchanged");
        assert!(json.get("id").is_none());
    }
    #[test]
    fn rewrite_request_id_handles_string_ids() {
        let mut json = pv(r#"{"jsonrpc":"2.0","method":"test","id":"abc-123"}"#);
        let (namespaced_id, original_id) = rewrite_request_id(&mut json, ClientId(456)).unwrap();
        assert_eq!(original_id, serde_json::json!("abc-123"));
        assert_eq!(namespaced_id, "456|\"abc-123\"");
        assert_eq!(json["id"], "456|\"abc-123\"");
    }
    #[test]
    fn inject_capabilities_adds_yolo_mode_to_session_new() {
        let payload = format!(
            r#"{{"jsonrpc":"2.0","method":"{}","id":1,"params":{{"cwd":"/tmp"}}}}"#,
            AGENT_METHOD_NAMES.session_new
        );
        let caps = ClientCapabilities {
            yolo_mode: true,
            default_model: None,
            ..Default::default()
        };
        let mut json = pv(&payload);
        assert!(inject_capabilities_into_session_new(
            &mut json,
            &caps,
            "",
            ClientId(1)
        ));
        assert_eq!(json["params"]["_meta"]["yoloMode"], true);
    }
    /// Leader capabilities.auto_mode seeds `_meta.autoMode` on session/new
    /// (the real ConnectFlags.default_auto_mode entry path).
    #[test]
    fn inject_capabilities_adds_auto_mode_to_session_new() {
        let payload = format!(
            r#"{{"jsonrpc":"2.0","method":"{}","id":1,"params":{{"cwd":"/tmp"}}}}"#,
            AGENT_METHOD_NAMES.session_new
        );
        let caps = ClientCapabilities {
            auto_mode: true,
            yolo_mode: false,
            default_model: None,
            ..Default::default()
        };
        let mut json = pv(&payload);
        assert!(inject_capabilities_into_session_new(
            &mut json,
            &caps,
            "",
            ClientId(1)
        ));
        assert_eq!(json["params"]["_meta"]["autoMode"], true);
        assert!(json["params"]["_meta"].get("yoloMode").is_none());
    }
    /// session/load also receives autoMode (reconnect path).
    #[test]
    fn inject_capabilities_adds_auto_mode_to_session_load() {
        let payload = format!(
            r#"{{"jsonrpc":"2.0","method":"{}","id":1,"params":{{"sessionId":"sess-1"}}}}"#,
            AGENT_METHOD_NAMES.session_load
        );
        let caps = ClientCapabilities {
            auto_mode: true,
            yolo_mode: false,
            ..Default::default()
        };
        let mut json = pv(&payload);
        assert!(inject_capabilities_into_session_new(
            &mut json,
            &caps,
            "grok-tui",
            ClientId(1)
        ));
        assert_eq!(json["params"]["_meta"]["autoMode"], true);
    }
    /// Yolo suppresses autoMode injection even when auto_mode capability is set.
    #[test]
    fn inject_capabilities_yolo_suppresses_auto_mode() {
        let payload = format!(
            r#"{{"jsonrpc":"2.0","method":"{}","id":1,"params":{{"cwd":"/tmp"}}}}"#,
            AGENT_METHOD_NAMES.session_new
        );
        let caps = ClientCapabilities {
            auto_mode: true,
            yolo_mode: true,
            ..Default::default()
        };
        let mut json = pv(&payload);
        assert!(inject_capabilities_into_session_new(
            &mut json,
            &caps,
            "",
            ClientId(1)
        ));
        assert_eq!(json["params"]["_meta"]["yoloMode"], true);
        assert!(
            json["params"]["_meta"].get("autoMode").is_none(),
            "yolo must not also inject autoMode"
        );
    }
    #[test]
    fn inject_capabilities_skips_non_session_new() {
        let mut json = pv(r#"{"jsonrpc":"2.0","method":"other/method","id":1,"params":{}}"#);
        let caps = ClientCapabilities {
            yolo_mode: true,
            default_model: None,
            ..Default::default()
        };
        assert!(!inject_capabilities_into_session_new(
            &mut json,
            &caps,
            "",
            ClientId(1)
        ));
        assert!(json["params"].get("_meta").is_none());
    }
    #[test]
    fn inject_capabilities_skips_when_yolo_mode_false() {
        let payload = format!(
            r#"{{"jsonrpc":"2.0","method":"{}","id":1,"params":{{"cwd":"/tmp"}}}}"#,
            AGENT_METHOD_NAMES.session_new
        );
        let caps = ClientCapabilities {
            yolo_mode: false,
            default_model: None,
            ..Default::default()
        };
        let mut json = pv(&payload);
        let before = json.clone();
        assert!(!inject_capabilities_into_session_new(
            &mut json,
            &caps,
            "",
            ClientId(1)
        ));
        assert_eq!(json, before);
    }
    #[test]
    fn inject_capabilities_preserves_existing_meta() {
        let payload = format!(
            r#"{{"jsonrpc":"2.0","method":"{}","id":1,"params":{{"cwd":"/tmp","_meta":{{"foo":"bar"}}}}}}"#,
            AGENT_METHOD_NAMES.session_new
        );
        let caps = ClientCapabilities {
            yolo_mode: true,
            default_model: None,
            ..Default::default()
        };
        let mut json = pv(&payload);
        assert!(inject_capabilities_into_session_new(
            &mut json,
            &caps,
            "",
            ClientId(1)
        ));
        assert_eq!(json["params"]["_meta"]["foo"], "bar");
        assert_eq!(json["params"]["_meta"]["yoloMode"], true);
    }
    #[test]
    fn inject_capabilities_adds_default_model_to_session_new() {
        let payload = format!(
            r#"{{"jsonrpc":"2.0","method":"{}","id":1,"params":{{"cwd":"/tmp"}}}}"#,
            AGENT_METHOD_NAMES.session_new
        );
        let caps = ClientCapabilities {
            yolo_mode: false,
            default_model: Some("grok-3-fast".to_string()),
            ..Default::default()
        };
        let mut json = pv(&payload);
        assert!(inject_capabilities_into_session_new(
            &mut json,
            &caps,
            "",
            ClientId(1)
        ));
        assert_eq!(json["params"]["_meta"]["modelId"], "grok-3-fast");
        assert!(json["params"]["_meta"].get("yoloMode").is_none());
    }
    #[test]
    fn inject_capabilities_adds_both_yolo_and_model() {
        let payload = format!(
            r#"{{"jsonrpc":"2.0","method":"{}","id":1,"params":{{"cwd":"/tmp"}}}}"#,
            AGENT_METHOD_NAMES.session_new
        );
        let caps = ClientCapabilities {
            yolo_mode: true,
            default_model: Some("grok-3-fast".to_string()),
            ..Default::default()
        };
        let mut json = pv(&payload);
        assert!(inject_capabilities_into_session_new(
            &mut json,
            &caps,
            "",
            ClientId(1)
        ));
        assert_eq!(json["params"]["_meta"]["yoloMode"], true);
        assert_eq!(json["params"]["_meta"]["modelId"], "grok-3-fast");
    }
    #[test]
    fn inject_capabilities_does_not_override_existing_model_id() {
        let payload = format!(
            r#"{{"jsonrpc":"2.0","method":"{}","id":1,"params":{{"cwd":"/tmp","_meta":{{"modelId":"custom-model"}}}}}}"#,
            AGENT_METHOD_NAMES.session_new
        );
        let caps = ClientCapabilities {
            yolo_mode: false,
            default_model: Some("grok-3-fast".to_string()),
            ..Default::default()
        };
        let mut json = pv(&payload);
        inject_capabilities_into_session_new(&mut json, &caps, "", ClientId(1));
        assert_eq!(json["params"]["_meta"]["modelId"], "custom-model");
    }
    #[test]
    fn extract_yolo_mode_change_returns_value() {
        let payload =
            r#"{"jsonrpc":"2.0","method":"x.ai/yolo_mode_changed","params":{"yolo_mode":true}}"#;
        assert_eq!(extract_yolo_mode_change(&pv(payload)), Some(true));
        let payload =
            r#"{"jsonrpc":"2.0","method":"x.ai/yolo_mode_changed","params":{"yolo_mode":false}}"#;
        assert_eq!(extract_yolo_mode_change(&pv(payload)), Some(false));
    }
    #[test]
    fn extract_yolo_mode_change_returns_none_for_other_methods() {
        let payload = r#"{"jsonrpc":"2.0","method":"other/method","params":{"yolo_mode":true}}"#;
        assert_eq!(extract_yolo_mode_change(&pv(payload)), None);
    }
    /// Branch 1: an explicit `auto_mode` flag wins, even over `permission_mode`.
    #[test]
    fn extract_auto_mode_change_explicit_flag_wins() {
        let payload =
            r#"{"jsonrpc":"2.0","method":"x.ai/yolo_mode_changed","params":{"auto_mode":true}}"#;
        assert_eq!(extract_auto_mode_change(&pv(payload)), Some(true));
        let payload =
            r#"{"jsonrpc":"2.0","method":"x.ai/yolo_mode_changed","params":{"auto_mode":false}}"#;
        assert_eq!(extract_auto_mode_change(&pv(payload)), Some(false));
        let payload = r#"{"jsonrpc":"2.0","method":"x.ai/yolo_mode_changed","params":{"auto_mode":false,"permission_mode":"auto"}}"#;
        assert_eq!(extract_auto_mode_change(&pv(payload)), Some(false));
    }
    /// Branch 2: with no explicit flag, derive from `permission_mode`.
    #[test]
    fn extract_auto_mode_change_derives_from_permission_mode() {
        let payload = r#"{"jsonrpc":"2.0","method":"x.ai/yolo_mode_changed","params":{"permission_mode":"auto"}}"#;
        assert_eq!(extract_auto_mode_change(&pv(payload)), Some(true));
        for mode in ["ask", "always-approve", "default"] {
            let payload = format!(
                r#"{{"jsonrpc":"2.0","method":"x.ai/yolo_mode_changed","params":{{"permission_mode":"{mode}"}}}}"#
            );
            assert_eq!(
                extract_auto_mode_change(&pv(&payload)),
                Some(false),
                "permission_mode={mode} must clear auto"
            );
        }
    }
    /// Branch 3: None when there's no auto signal — wrong method, or a bare yolo
    /// toggle (no `auto_mode`, no `permission_mode`) must NOT change auto state.
    #[test]
    fn extract_auto_mode_change_returns_none_when_no_auto_signal() {
        let payload = r#"{"jsonrpc":"2.0","method":"other/method","params":{"auto_mode":true}}"#;
        assert_eq!(extract_auto_mode_change(&pv(payload)), None);
        let payload =
            r#"{"jsonrpc":"2.0","method":"x.ai/yolo_mode_changed","params":{"yolo_mode":true}}"#;
        assert_eq!(extract_auto_mode_change(&pv(payload)), None);
    }
    #[test]
    fn extract_model_id_from_set_model_returns_value() {
        let payload = format!(
            r#"{{"jsonrpc":"2.0","method":"{}","id":1,"params":{{"sessionId":"sess-123","modelId":"grok-3-fast"}}}}"#,
            AGENT_METHOD_NAMES.session_set_model
        );
        assert_eq!(
            extract_model_id_from_set_model(&pv(&payload)),
            Some("grok-3-fast".to_string())
        );
    }
    #[test]
    fn extract_model_id_from_set_model_handles_snake_case() {
        let payload = format!(
            r#"{{"jsonrpc":"2.0","method":"{}","id":1,"params":{{"session_id":"sess-123","model_id":"grok-3"}}}}"#,
            AGENT_METHOD_NAMES.session_set_model
        );
        assert_eq!(
            extract_model_id_from_set_model(&pv(&payload)),
            Some("grok-3".to_string())
        );
    }
    #[test]
    fn extract_model_id_from_set_model_returns_none_for_other_methods() {
        let payload =
            r#"{"jsonrpc":"2.0","method":"other/method","id":1,"params":{"modelId":"grok-3"}}"#;
        assert_eq!(extract_model_id_from_set_model(&pv(payload)), None);
    }
    #[test]
    fn extract_model_id_from_set_model_returns_none_for_empty_model() {
        let payload = format!(
            r#"{{"jsonrpc":"2.0","method":"{}","id":1,"params":{{"sessionId":"sess-123","modelId":""}}}}"#,
            AGENT_METHOD_NAMES.session_set_model
        );
        assert_eq!(extract_model_id_from_set_model(&pv(&payload)), None);
    }
    #[test]
    fn extract_model_id_from_set_model_returns_none_for_missing_model() {
        let payload = format!(
            r#"{{"jsonrpc":"2.0","method":"{}","id":1,"params":{{"sessionId":"sess-123"}}}}"#,
            AGENT_METHOD_NAMES.session_set_model
        );
        assert_eq!(extract_model_id_from_set_model(&pv(&payload)), None);
    }
    #[test]
    fn patch_initialize_response_patches_current_model_id() {
        let mut json = pv(
            r#"{"jsonrpc":"2.0","id":1,"result":{"meta":{"modelState":{"currentModelId":"grok-3","availableModels":[]}}}}"#,
        );
        let default_model = Some("grok-3-fast".to_string());
        assert!(patch_initialize_response_model(&mut json, &default_model));
        assert_eq!(
            json["result"]["meta"]["modelState"]["currentModelId"],
            "grok-3-fast"
        );
    }
    #[test]
    fn patch_initialize_response_preserves_other_fields() {
        let mut json = pv(
            r#"{"jsonrpc":"2.0","id":1,"result":{"meta":{"grokShell":true,"modelState":{"currentModelId":"grok-3","availableModels":[{"modelId":"grok-3"},{"modelId":"grok-3-fast"}]}}}}"#,
        );
        let default_model = Some("grok-3-fast".to_string());
        assert!(patch_initialize_response_model(&mut json, &default_model));
        assert_eq!(json["result"]["meta"]["grokShell"], true);
        assert_eq!(
            json["result"]["meta"]["modelState"]["currentModelId"],
            "grok-3-fast"
        );
        assert_eq!(
            json["result"]["meta"]["modelState"]["availableModels"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }
    #[test]
    fn patch_initialize_response_noop_when_no_default_model() {
        let mut json = pv(
            r#"{"jsonrpc":"2.0","id":1,"result":{"meta":{"modelState":{"currentModelId":"grok-3"}}}}"#,
        );
        let before = json.clone();
        assert!(!patch_initialize_response_model(&mut json, &None));
        assert_eq!(json, before);
    }
    #[test]
    fn patch_initialize_response_noop_when_empty_default_model() {
        let mut json = pv(
            r#"{"jsonrpc":"2.0","id":1,"result":{"meta":{"modelState":{"currentModelId":"grok-3"}}}}"#,
        );
        let before = json.clone();
        assert!(!patch_initialize_response_model(
            &mut json,
            &Some("".to_string())
        ));
        assert_eq!(json, before);
    }
    #[test]
    fn patch_initialize_response_noop_when_already_matches() {
        let mut json = pv(
            r#"{"jsonrpc":"2.0","id":1,"result":{"meta":{"modelState":{"currentModelId":"grok-3"}}}}"#,
        );
        let before = json.clone();
        assert!(!patch_initialize_response_model(
            &mut json,
            &Some("grok-3".to_string())
        ));
        assert_eq!(json, before);
    }
    #[test]
    fn patch_initialize_response_noop_for_non_initialize_response() {
        let mut json = pv(
            r#"{"jsonrpc":"2.0","id":1,"result":{"session_id":"sess-1","models":{"currentModelId":"grok-3","availableModels":[]}}}"#,
        );
        let before = json.clone();
        assert!(!patch_initialize_response_model(
            &mut json,
            &Some("grok-3-fast".to_string())
        ));
        assert_eq!(json, before);
    }
    #[test]
    fn extract_session_id_from_result_works() {
        let payload = r#"{"jsonrpc":"2.0","result":{"session_id":"sess-123"},"id":1}"#;
        assert_eq!(
            extract_session_id_from_result(&pv(payload)),
            Some("sess-123".to_string())
        );
        let payload = r#"{"jsonrpc":"2.0","result":{"sessionId":"sess-456"},"id":1}"#;
        assert_eq!(
            extract_session_id_from_result(&pv(payload)),
            Some("sess-456".to_string())
        );
    }
    #[test]
    fn extract_session_id_from_result_returns_none_for_other_responses() {
        let payload = r#"{"jsonrpc":"2.0","result":{"other":"value"},"id":1}"#;
        assert_eq!(extract_session_id_from_result(&pv(payload)), None);
        let payload = r#"{"jsonrpc":"2.0","error":{"code":-1,"message":"fail"},"id":1}"#;
        assert_eq!(extract_session_id_from_result(&pv(payload)), None);
        let payload = r#"{"jsonrpc":"2.0","method":"test","params":{"session_id":"abc"},"id":1}"#;
        assert_eq!(extract_session_id_from_result(&pv(payload)), None);
    }
    #[test]
    fn extract_session_id_from_params_works() {
        let payload = r#"{"jsonrpc":"2.0","method":"session/notification","params":{"session_id":"sess-789"}}"#;
        assert_eq!(
            extract_session_id(&pv(payload)),
            Some("sess-789".to_string())
        );
        let payload = r#"{"jsonrpc":"2.0","method":"session/notification","params":{"sessionId":"sess-abc"}}"#;
        assert_eq!(
            extract_session_id(&pv(payload)),
            Some("sess-abc".to_string())
        );
    }
    #[test]
    fn extract_session_id_from_nested_params_works() {
        let payload = r#"{"jsonrpc":"2.0","method":"_x.ai/session_notification","params":{"method":"x.ai/session_notification","params":{"sessionId":"sess-nested"}}}"#;
        assert_eq!(
            extract_session_id(&pv(payload)),
            Some("sess-nested".to_string())
        );
        let payload = r#"{"jsonrpc":"2.0","method":"_x.ai/fs_notify","params":{"method":"x.ai/fs_notify","params":{"session_id":"sess-nested-2","event":{}}}}"#;
        assert_eq!(
            extract_session_id(&pv(payload)),
            Some("sess-nested-2".to_string())
        );
        let payload = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"top-level","params":{"sessionId":"nested"}}}"#;
        assert_eq!(
            extract_session_id(&pv(payload)),
            Some("top-level".to_string())
        );
    }
    #[test]
    fn extract_session_id_from_prompt_complete_works() {
        let payload = r#"{"jsonrpc":"2.0","method":"x.ai/session/prompt_complete","params":{"sessionId":"sess-prompt"}}"#;
        assert_eq!(
            extract_session_id_from_prompt_complete(&pv(payload)),
            Some("sess-prompt".to_string())
        );
    }
    #[test]
    fn extract_session_id_from_prompt_complete_ignores_other_methods() {
        let payload = r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"sess-prompt"}}"#;
        assert_eq!(extract_session_id_from_prompt_complete(&pv(payload)), None);
    }
    #[test]
    fn extract_child_session_event_spawned() {
        let payload = r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"parent","update":{"sessionUpdate":"subagent_spawned","child_session_id":"child-1"}}}"#;
        match extract_child_session_event(&pv(payload)) {
            Some(ChildSessionEvent::Spawned(id)) => assert_eq!(id, "child-1"),
            other => panic!("Expected Spawned, got {:?}", other),
        }
    }
    #[test]
    fn extract_child_session_event_finished() {
        let payload = r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"parent","update":{"sessionUpdate":"subagent_finished","child_session_id":"child-2"}}}"#;
        match extract_child_session_event(&pv(payload)) {
            Some(ChildSessionEvent::Finished(id)) => assert_eq!(id, "child-2"),
            other => panic!("Expected Finished, got {:?}", other),
        }
    }
    #[test]
    fn extract_child_session_event_nested_ext_notification() {
        let payload = r#"{"jsonrpc":"2.0","method":"_x.ai/session_notification","params":{"method":"x.ai/session_notification","params":{"sessionId":"parent","update":{"sessionUpdate":"subagent_spawned","child_session_id":"child-3"}}}}"#;
        match extract_child_session_event(&pv(payload)) {
            Some(ChildSessionEvent::Spawned(id)) => assert_eq!(id, "child-3"),
            other => panic!("Expected Spawned, got {:?}", other),
        }
    }
    #[test]
    fn extract_child_session_event_nested_ext_notification_finished() {
        let payload = r#"{"jsonrpc":"2.0","method":"_x.ai/session_notification","params":{"method":"x.ai/session_notification","params":{"sessionId":"parent","update":{"sessionUpdate":"subagent_finished","child_session_id":"child-4"}}}}"#;
        match extract_child_session_event(&pv(payload)) {
            Some(ChildSessionEvent::Finished(id)) => assert_eq!(id, "child-4"),
            other => panic!("Expected Finished, got {:?}", other),
        }
    }
    #[test]
    fn extract_child_session_event_none_for_other_updates() {
        let payload = r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"parent","update":{"sessionUpdate":"message_delta","content":"hello"}}}"#;
        assert!(extract_child_session_event(&pv(payload)).is_none());
    }
    #[test]
    fn extract_child_session_event_none_without_child_id() {
        let payload = r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"parent","update":{"sessionUpdate":"subagent_spawned"}}}"#;
        assert!(extract_child_session_event(&pv(payload)).is_none());
    }
    #[test]
    fn inject_capabilities_skips_empty_default_model() {
        let payload = format!(
            r#"{{"jsonrpc":"2.0","method":"{}","id":1,"params":{{"cwd":"/tmp"}}}}"#,
            AGENT_METHOD_NAMES.session_new
        );
        let caps = ClientCapabilities {
            yolo_mode: false,
            default_model: Some("".to_string()),
            ..Default::default()
        };
        let mut json = pv(&payload);
        let before = json.clone();
        assert!(!inject_capabilities_into_session_new(
            &mut json,
            &caps,
            "",
            ClientId(1)
        ));
        assert_eq!(json, before);
    }
    #[test]
    fn inject_capabilities_skips_empty_model_with_yolo_mode() {
        let payload = format!(
            r#"{{"jsonrpc":"2.0","method":"{}","id":1,"params":{{"cwd":"/tmp"}}}}"#,
            AGENT_METHOD_NAMES.session_new
        );
        let caps = ClientCapabilities {
            yolo_mode: true,
            default_model: Some("".to_string()),
            ..Default::default()
        };
        let mut json = pv(&payload);
        assert!(inject_capabilities_into_session_new(
            &mut json,
            &caps,
            "",
            ClientId(1)
        ));
        assert_eq!(json["params"]["_meta"]["yoloMode"], true);
        assert!(json["params"]["_meta"].get("modelId").is_none());
    }
    #[test]
    fn inject_capabilities_no_model_no_yolo_returns_unchanged() {
        let payload = format!(
            r#"{{"jsonrpc":"2.0","method":"{}","id":1,"params":{{"cwd":"/tmp","_meta":{{"yoloMode":true}}}}}}"#,
            AGENT_METHOD_NAMES.session_new
        );
        let caps = ClientCapabilities {
            yolo_mode: false,
            default_model: None,
            ..Default::default()
        };
        let mut json = pv(&payload);
        let before = json.clone();
        assert!(!inject_capabilities_into_session_new(
            &mut json,
            &caps,
            "",
            ClientId(1)
        ));
        assert_eq!(json, before);
    }
    #[test]
    fn inject_capabilities_adds_client_identifier_to_session_new() {
        let payload = format!(
            r#"{{"jsonrpc":"2.0","method":"{}","id":1,"params":{{"cwd":"/tmp"}}}}"#,
            AGENT_METHOD_NAMES.session_new
        );
        let caps = ClientCapabilities::default();
        let mut json = pv(&payload);
        assert!(inject_capabilities_into_session_new(
            &mut json,
            &caps,
            "grok-code-extension",
            ClientId(1),
        ));
        assert_eq!(
            json["params"]["_meta"]["clientIdentifier"],
            "grok-code-extension"
        );
    }
    #[test]
    fn inject_capabilities_does_not_override_existing_client_identifier() {
        let payload = format!(
            r#"{{"jsonrpc":"2.0","method":"{}","id":1,"params":{{"cwd":"/tmp","_meta":{{"clientIdentifier":"custom-client"}}}}}}"#,
            AGENT_METHOD_NAMES.session_new
        );
        let caps = ClientCapabilities::default();
        let mut json = pv(&payload);
        inject_capabilities_into_session_new(&mut json, &caps, "grok-tui", ClientId(1));
        assert_eq!(json["params"]["_meta"]["clientIdentifier"], "custom-client");
    }
    #[test]
    fn inject_capabilities_adds_client_identifier_to_session_load() {
        let payload = format!(
            r#"{{"jsonrpc":"2.0","method":"{}","id":1,"params":{{"sessionId":"sess-1"}}}}"#,
            AGENT_METHOD_NAMES.session_load
        );
        let caps = ClientCapabilities::default();
        let mut json = pv(&payload);
        assert!(inject_capabilities_into_session_new(
            &mut json,
            &caps,
            "grok-code-extension",
            ClientId(1),
        ));
        assert_eq!(
            json["params"]["_meta"]["clientIdentifier"],
            "grok-code-extension"
        );
        assert!(json["params"]["_meta"].get("yoloMode").is_none());
        assert!(json["params"]["_meta"].get("modelId").is_none());
    }
    #[test]
    fn inject_capabilities_adds_leader_client_id_to_session_load() {
        let payload = format!(
            r#"{{"jsonrpc":"2.0","method":"{}","id":1,"params":{{"sessionId":"sess-1"}}}}"#,
            AGENT_METHOD_NAMES.session_load
        );
        let caps = ClientCapabilities::default();
        let mut json = pv(&payload);
        inject_capabilities_into_session_new(&mut json, &caps, "grok-tui", ClientId(42));
        assert_eq!(
            json["params"]["_meta"]["x.ai/leaderClientId"].as_u64(),
            Some(42)
        );
    }
    #[test]
    fn inject_capabilities_does_not_override_existing_leader_client_id() {
        let payload = format!(
            r#"{{"jsonrpc":"2.0","method":"{}","id":1,"params":{{"sessionId":"sess-1","_meta":{{"x.ai/leaderClientId":7}}}}}}"#,
            AGENT_METHOD_NAMES.session_load
        );
        let caps = ClientCapabilities::default();
        let mut json = pv(&payload);
        inject_capabilities_into_session_new(&mut json, &caps, "grok-tui", ClientId(42));
        assert_eq!(
            json["params"]["_meta"]["x.ai/leaderClientId"].as_u64(),
            Some(7)
        );
    }
    #[test]
    fn extract_target_client_id_some_when_meta_present() {
        let direct = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","_meta":{"x.ai/leaderClientId":9}}}"#;
        assert_eq!(extract_target_client_id(&pv(direct)), Some(ClientId(9)));
        let nested = r#"{"jsonrpc":"2.0","method":"_x.ai/session/update","params":{"params":{"sessionId":"sess-1","_meta":{"x.ai/leaderClientId":11}}}}"#;
        assert_eq!(extract_target_client_id(&pv(nested)), Some(ClientId(11)));
    }
    #[test]
    fn extract_target_client_id_none_when_absent() {
        let no_meta =
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1"}}"#;
        assert_eq!(extract_target_client_id(&pv(no_meta)), None);
        let no_key = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","_meta":{"isReplay":true}}}"#;
        assert_eq!(extract_target_client_id(&pv(no_key)), None);
    }
    #[test]
    fn inject_yolo_notification_adds_client_identifier() {
        let mut json = pv(
            r#"{"jsonrpc":"2.0","method":"x.ai/yolo_mode_changed","params":{"yolo_mode":true}}"#,
        );
        assert!(inject_client_identity_into_yolo_notification(
            &mut json, "grok-tui"
        ));
        assert_eq!(json["params"]["clientIdentifier"], "grok-tui");
        assert_eq!(json["params"]["yolo_mode"], true);
    }
    #[test]
    fn inject_yolo_notification_skips_non_yolo_methods() {
        let mut json = pv(r#"{"jsonrpc":"2.0","method":"x.ai/other","params":{"data":1}}"#);
        let before = json.clone();
        assert!(!inject_client_identity_into_yolo_notification(
            &mut json, "grok-tui"
        ));
        assert_eq!(json, before);
    }
    #[test]
    fn inject_client_identity_adds_identifier_to_initialize() {
        let mut json = pv(
            r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"protocolVersion":"0.1"}}"#,
        );
        let (mutated, was_initialize) =
            inject_client_identity_into_initialize(&mut json, "grok-tui");
        assert!(was_initialize, "should have detected an initialize message");
        assert!(mutated, "should have injected the identifier");
        assert_eq!(json["params"]["_meta"]["clientIdentifier"], "grok-tui");
    }
    #[test]
    fn inject_client_identity_does_not_override_existing() {
        let mut json = pv(
            r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"protocolVersion":"0.1","_meta":{"clientIdentifier":"grok-web"}}}"#,
        );
        let (mutated, was_initialize) =
            inject_client_identity_into_initialize(&mut json, "grok-tui");
        assert!(was_initialize, "should have detected an initialize message");
        assert!(!mutated, "existing identifier means nothing was injected");
        assert_eq!(json["params"]["_meta"]["clientIdentifier"], "grok-web");
    }
    #[test]
    fn inject_client_identity_skips_non_initialize() {
        let mut json =
            pv(r#"{"jsonrpc":"2.0","method":"session/new","id":1,"params":{"cwd":"/tmp"}}"#);
        let before = json.clone();
        let (mutated, was_initialize) =
            inject_client_identity_into_initialize(&mut json, "grok-tui");
        assert!(
            !was_initialize,
            "session/new should not be detected as initialize"
        );
        assert!(!mutated);
        assert_eq!(json, before);
    }
    #[test]
    fn inject_client_identity_skips_empty_client_type() {
        let mut json = pv(
            r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"protocolVersion":"0.1"}}"#,
        );
        let before = json.clone();
        let (mutated, was_initialize) = inject_client_identity_into_initialize(&mut json, "");
        assert!(
            !was_initialize,
            "empty client_type means no injection, not an initialize"
        );
        assert!(!mutated);
        assert_eq!(json, before);
    }
    #[test]
    fn inject_client_identity_preserves_existing_meta() {
        let mut json = pv(
            r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"protocolVersion":"0.1","_meta":{"foo":"bar"}}}"#,
        );
        let (mutated, was_initialize) =
            inject_client_identity_into_initialize(&mut json, "grok-code-extension");
        assert!(was_initialize, "should have detected an initialize message");
        assert!(mutated);
        assert_eq!(json["params"]["_meta"]["foo"], "bar");
        assert_eq!(
            json["params"]["_meta"]["clientIdentifier"],
            "grok-code-extension"
        );
    }
    #[test]
    fn version_mismatch_notification_contains_correct_fields() {
        let payload = make_version_mismatch_notification("0.1.157", "0.1.150")
            .expect("should produce notification");
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["method"], "x.ai/leader/version_mismatch");
        assert_eq!(json["params"]["clientVersion"], "0.1.157");
        assert_eq!(json["params"]["leaderVersion"], "0.1.150");
        assert!(
            json["params"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("0.1.157"),
            "message should mention the client version"
        );
    }
    #[test]
    fn version_mismatch_notification_is_none_when_versions_match() {
        assert!(
            make_version_mismatch_notification("0.1.150", "0.1.150").is_none(),
            "matching versions must not produce a notification"
        );
    }
    #[test]
    fn version_mismatch_notification_is_none_for_unknown_leader_version() {
        assert!(
            make_version_mismatch_notification("0.1.150", "unknown").is_none(),
            "unknown leader version (dev build) must not produce a notification"
        );
    }
    /// Verify that a session/setModel request updates the client's default_model
    /// capability, so the next session/new injects the updated model.
    #[tokio::test]
    async fn set_model_updates_default_model_for_next_session_new() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, mut acp_rx) = setup_test_server(&temp).await;
        let stream = LeaderStream::connect(&sock_path).await.unwrap();
        let (mut reader, mut writer) = tokio::io::split(stream);
        write_message(
            &mut writer,
            &ClientMessage::Register {
                client_type: "test".into(),
                mode: ClientMode::Stdio,
                capabilities: ClientCapabilities {
                    yolo_mode: false,
                    default_model: Some("grok-original".to_string()),
                    ..Default::default()
                },
            },
        )
        .await
        .unwrap();
        let _: ServerMessage = read_message(&mut reader).await.unwrap();
        let set_model_payload = format!(
            r#"{{"jsonrpc":"2.0","method":"{}","id":1,"params":{{"sessionId":"sess-1","modelId":"grok-4.5"}}}}"#,
            AGENT_METHOD_NAMES.session_set_model
        );
        write_message(
            &mut writer,
            &ClientMessage::Acp {
                payload: set_model_payload,
            },
        )
        .await
        .unwrap();
        let _ = acp_rx.recv().await.unwrap();
        let session_new_payload = format!(
            r#"{{"jsonrpc":"2.0","method":"{}","id":2,"params":{{"cwd":"/tmp"}}}}"#,
            AGENT_METHOD_NAMES.session_new
        );
        write_message(
            &mut writer,
            &ClientMessage::Acp {
                payload: session_new_payload,
            },
        )
        .await
        .unwrap();
        let forwarded = acp_rx.recv().await.unwrap();
        let json: serde_json::Value = serde_json::from_str(&forwarded).unwrap();
        assert_eq!(
            json["params"]["_meta"]["modelId"], "grok-4.5",
            "Leader should inject the updated model after session/setModel, not the stale registration model"
        );
        cancel.cancel();
    }
    #[tokio::test]
    async fn client_count_starts_at_zero() {
        let temp = TempDir::new().unwrap();
        let (_sock_path, cancel, _acp_rx, client_count) =
            setup_test_server_with_client_count(&temp).await;
        assert_eq!(
            client_count.load(Ordering::Relaxed),
            0,
            "client_count should start at 0"
        );
        cancel.cancel();
    }
    #[tokio::test]
    async fn client_count_increments_on_connect() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, _acp_rx, client_count) =
            setup_test_server_with_client_count(&temp).await;
        let (_reader1, _writer1) = connect_and_register(&sock_path, "client-1").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            client_count.load(Ordering::Relaxed),
            1,
            "client_count should be 1 after one client connects"
        );
        let (_reader2, _writer2) = connect_and_register(&sock_path, "client-2").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            client_count.load(Ordering::Relaxed),
            2,
            "client_count should be 2 after two clients connect"
        );
        cancel.cancel();
    }
    #[tokio::test]
    async fn client_count_decrements_on_disconnect() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, _acp_rx, client_count) =
            setup_test_server_with_client_count(&temp).await;
        let (_reader1, mut writer1) = connect_and_register(&sock_path, "client-1").await;
        let (_reader2, _writer2) = connect_and_register(&sock_path, "client-2").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(client_count.load(Ordering::Relaxed), 2);
        write_message(&mut writer1, &ClientMessage::Disconnect)
            .await
            .unwrap();
        drop(_reader1);
        drop(writer1);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            client_count.load(Ordering::Relaxed),
            1,
            "client_count should be 1 after one client disconnects"
        );
        cancel.cancel();
    }
    #[tokio::test]
    async fn client_count_returns_to_zero_after_all_disconnect() {
        let temp = TempDir::new().unwrap();
        let sock_path = temp.path().join("test.sock");
        let (acp_tx, _acp_rx) = mpsc::unbounded_channel();
        let (_response_tx, response_rx) = mpsc::unbounded_channel();
        let server_cancel = CancellationToken::new();
        let client_count = Arc::new(AtomicUsize::new(0));
        let agent_busy = Arc::new(AtomicBool::new(false));
        let control_state = default_test_control_state(&sock_path);
        let sock_clone = sock_path.clone();
        let cancel_clone = server_cancel.clone();
        let count_clone = client_count.clone();
        let busy_clone = agent_busy.clone();
        tokio::spawn(async move {
            let _ = run_leader_server(
                sock_clone,
                acp_tx,
                response_rx,
                cancel_clone,
                true,
                count_clone,
                busy_clone,
                AgentActivity::default(),
                watch::channel(true).1,
                watch::channel(false).0,
                watch::channel(super::super::protocol::ShutdownReason::Manual).0,
                None,
                control_state,
            )
            .await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        {
            let (_reader, mut writer) = connect_and_register(&sock_path, "temp-client").await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(client_count.load(Ordering::Relaxed), 1);
            write_message(&mut writer, &ClientMessage::Disconnect)
                .await
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            client_count.load(Ordering::Relaxed),
            0,
            "client_count should return to 0 after all clients disconnect"
        );
        server_cancel.cancel();
    }
    #[tokio::test]
    async fn client_count_not_incremented_before_registration() {
        let temp = TempDir::new().unwrap();
        let (_sock_path, cancel, _acp_rx, client_count) =
            setup_test_server_with_client_count(&temp).await;
        let _stream = LeaderStream::connect(&_sock_path).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            client_count.load(Ordering::Relaxed),
            0,
            "client_count should remain 0 for unregistered connections"
        );
        cancel.cancel();
    }
    #[tokio::test]
    async fn fallback_routing_forwards_notifications_but_drops_responses() {
        let temp = TempDir::new().unwrap();
        let sock_path = temp.path().join("test.sock");
        let (acp_tx, _acp_rx) = mpsc::unbounded_channel();
        let (response_tx, response_rx) = mpsc::unbounded_channel();
        let server_cancel = CancellationToken::new();
        let client_count = Arc::new(AtomicUsize::new(0));
        let control_state = default_test_control_state(&sock_path);
        let sock_clone = sock_path.clone();
        let cancel_clone = server_cancel.clone();
        let count_clone = client_count.clone();
        tokio::spawn(async move {
            let _ = run_leader_server(
                sock_clone,
                acp_tx,
                response_rx,
                cancel_clone,
                true,
                count_clone,
                Arc::new(AtomicBool::new(false)),
                AgentActivity::default(),
                watch::channel(true).1,
                watch::channel(false).0,
                watch::channel(super::super::protocol::ShutdownReason::Manual).0,
                None,
                control_state,
            )
            .await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stream = LeaderStream::connect(&sock_path).await.unwrap();
        let (mut reader, mut writer) = tokio::io::split(stream);
        write_message(
            &mut writer,
            &ClientMessage::Register {
                client_type: "test".into(),
                mode: ClientMode::Stdio,
                capabilities: ClientCapabilities::default(),
            },
        )
        .await
        .unwrap();
        let _: ServerMessage = read_message(&mut reader).await.unwrap();
        write_message(
            &mut writer,
            &ClientMessage::Acp {
                payload: r#"{"jsonrpc":"2.0","method":"test","id":99}"#.into(),
            },
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        response_tx
            .send(r#"{"jsonrpc":"2.0","result":{"ok":true},"id":42}"#.to_string())
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        response_tx
            .send(
                r#"{"jsonrpc":"2.0","method":"agent/progress","params":{"status":"working"}}"#
                    .to_string(),
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let msg: ServerMessage =
            tokio::time::timeout(Duration::from_millis(200), read_message(&mut reader))
                .await
                .expect("should receive notification")
                .unwrap();
        match msg {
            ServerMessage::Acp { payload } => {
                let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
                assert_eq!(
                    json["method"], "agent/progress",
                    "Should receive the notification, not the relay response"
                );
            }
            other => panic!("Expected Acp message, got {:?}", other),
        }
        server_cancel.cancel();
    }
    /// Relay-originated session notifications must be dropped, not forwarded
    /// to the last active IPC client.
    #[tokio::test]
    async fn relay_session_notification_not_forwarded_to_ipc_client() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx) = setup_persistent_server(&temp).await;
        let (mut reader, mut writer) = connect_and_register(&sock_path, "test").await;
        write_message(
            &mut writer,
            &ClientMessage::Acp {
                payload: r#"{"jsonrpc":"2.0","method":"test","id":99}"#.into(),
            },
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        response_tx
            .send(
                r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"relay-sess-xyz","data":"from-relay"}}"#
                    .into(),
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        response_tx
            .send(
                r#"{"jsonrpc":"2.0","method":"agent/progress","params":{"status":"working"}}"#
                    .into(),
            )
            .unwrap();
        let msg: ServerMessage =
            tokio::time::timeout(Duration::from_millis(200), read_message(&mut reader))
                .await
                .expect("should receive the session-less notification")
                .unwrap();
        match msg {
            ServerMessage::Acp { payload } => {
                let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
                assert_eq!(json["method"], "agent/progress");
                assert!(json["params"].get("sessionId").is_none());
            }
            other => panic!("Expected Acp message, got {:?}", other),
        }
        cancel.cancel();
    }
    /// When a client disconnects while its session streams, notifications for
    /// that session must NOT leak to another client via `last_active_client`.
    #[tokio::test]
    async fn dead_client_session_notification_not_leaked_to_other_client() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx) = setup_persistent_server(&temp).await;
        let (reader_a, mut writer_a) = connect_and_register(&sock_path, "test-a").await;
        write_message(
                &mut writer_a,
                &ClientMessage::Acp {
                    payload: r#"{"jsonrpc":"2.0","method":"session/prompt","id":1,"params":{"sessionId":"sess-A","prompt":[]}}"#
                        .into(),
                },
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(writer_a);
        drop(reader_a);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (mut reader_b, mut writer_b) = connect_and_register(&sock_path, "test-b").await;
        write_message(
            &mut writer_b,
            &ClientMessage::Acp {
                payload: r#"{"jsonrpc":"2.0","method":"initialize","id":2,"params":{}}"#.into(),
            },
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        response_tx
            .send(
                r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-A","sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"leaked content"}}}"#
                    .into(),
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        response_tx
            .send(
                r#"{"jsonrpc":"2.0","method":"agent/progress","params":{"status":"working"}}"#
                    .into(),
            )
            .unwrap();
        let msg: ServerMessage =
            tokio::time::timeout(Duration::from_millis(200), read_message(&mut reader_b))
                .await
                .expect("should receive the session-less notification")
                .unwrap();
        match msg {
            ServerMessage::Acp { payload } => {
                let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
                assert_eq!(json["method"], "agent/progress");
            }
            other => panic!("Expected Acp message, got {:?}", other),
        }
        cancel.cancel();
    }
    /// `ext/notification` with nested sessionId (params.params.sessionId) must
    /// route to the session owner, not fall through to `last_active_client`.
    #[tokio::test]
    async fn ext_notification_with_nested_session_id_routes_correctly() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx) = setup_persistent_server(&temp).await;
        let (mut reader_a, mut writer_a) = connect_and_register(&sock_path, "test-a").await;
        write_message(
                &mut writer_a,
                &ClientMessage::Acp {
                    payload: r#"{"jsonrpc":"2.0","method":"session/prompt","id":1,"params":{"sessionId":"sess-A","prompt":[]}}"#
                        .into(),
                },
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (mut reader_b, mut writer_b) = connect_and_register(&sock_path, "test-b").await;
        write_message(
            &mut writer_b,
            &ClientMessage::Acp {
                payload: r#"{"jsonrpc":"2.0","method":"initialize","id":2,"params":{}}"#.into(),
            },
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        response_tx
            .send(
                r#"{"jsonrpc":"2.0","method":"_x.ai/session_notification","params":{"method":"x.ai/session_notification","params":{"sessionId":"sess-A","update":{"sessionUpdate":"retry_state","attempt":1,"maxRetries":3,"reason":"transient"}}}}"#
                    .into(),
            )
            .unwrap();
        let msg: ServerMessage =
            tokio::time::timeout(Duration::from_millis(200), read_message(&mut reader_a))
                .await
                .expect("client A should receive the ext/notification")
                .unwrap();
        match msg {
            ServerMessage::Acp { payload } => {
                let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
                assert_eq!(json["method"], "_x.ai/session_notification");
            }
            other => panic!("Expected Acp message, got {:?}", other),
        }
        let timeout_result: Result<Result<ServerMessage, _>, _> =
            tokio::time::timeout(Duration::from_millis(100), read_message(&mut reader_b)).await;
        assert!(
            timeout_result.is_err(),
            "Client B should NOT receive session A's notification"
        );
        cancel.cancel();
    }
    #[tokio::test]
    async fn server_sends_shutting_down_before_shutdown() {
        let temp = TempDir::new().unwrap();
        let sock_path = temp.path().join("test.sock");
        let (acp_tx, _acp_rx) = mpsc::unbounded_channel();
        let (_response_tx, response_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let client_count = Arc::new(AtomicUsize::new(0));
        let control_state = default_test_control_state(&sock_path);
        let cancel_clone = cancel.clone();
        let sock_clone = sock_path.clone();
        let cc = client_count.clone();
        tokio::spawn(async move {
            let _ = run_leader_server(
                sock_clone,
                acp_tx,
                response_rx,
                cancel_clone,
                true,
                cc,
                Arc::new(AtomicBool::new(false)),
                AgentActivity::default(),
                watch::channel(true).1,
                watch::channel(false).0,
                watch::channel(super::super::protocol::ShutdownReason::Manual).0,
                None,
                control_state,
            )
            .await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (mut reader, _writer) = connect_and_register(&sock_path, "test").await;
        cancel.cancel();
        let msg1: ServerMessage =
            tokio::time::timeout(Duration::from_secs(5), read_message(&mut reader))
                .await
                .expect("should receive ShuttingDown")
                .unwrap();
        match msg1 {
            ServerMessage::ShuttingDown { reason, delay_ms } => {
                assert_eq!(
                    reason,
                    super::super::protocol::ShutdownReason::Manual,
                    "Reason should be Manual"
                );
                assert_eq!(delay_ms, 0, "delay_ms should be 0 (immediate shutdown)");
            }
            other => panic!("Expected ShuttingDown, got {:?}", other),
        }
        let msg2: ServerMessage =
            tokio::time::timeout(Duration::from_secs(5), read_message(&mut reader))
                .await
                .expect("should receive Shutdown")
                .unwrap();
        assert!(
            matches!(msg2, ServerMessage::Shutdown),
            "Expected Shutdown, got {:?}",
            msg2
        );
    }
    #[tokio::test]
    async fn agent_busy_set_when_request_forwarded() {
        let temp = TempDir::new().unwrap();
        let sock_path = temp.path().join("busy_test.sock");
        let mut handle = spawn_leader_server(sock_path.clone()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !handle.agent_busy.load(Ordering::Relaxed),
            "agent_busy should be false initially"
        );
        let stream = LeaderStream::connect(&sock_path).await.unwrap();
        let (mut reader, mut writer) = tokio::io::split(stream);
        write_message(
            &mut writer,
            &ClientMessage::Register {
                client_type: "test".into(),
                mode: ClientMode::Stdio,
                capabilities: ClientCapabilities::default(),
            },
        )
        .await
        .unwrap();
        let _: ServerMessage = read_message(&mut reader).await.unwrap();
        write_message(
            &mut writer,
            &ClientMessage::Acp {
                payload: r#"{"jsonrpc":"2.0","method":"test/ping","id":1}"#.into(),
            },
        )
        .await
        .unwrap();
        let forwarded = handle.acp_rx.recv().await.unwrap();
        assert!(forwarded.contains("test/ping"));
        assert!(
            handle.agent_busy.load(Ordering::Relaxed),
            "agent_busy should be true after forwarding a request"
        );
        handle.cancel.cancel();
    }
    #[tokio::test]
    async fn agent_busy_cleared_when_response_received() {
        let temp = TempDir::new().unwrap();
        let sock_path = temp.path().join("busy_clear.sock");
        let mut handle = spawn_leader_server(sock_path.clone()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stream = LeaderStream::connect(&sock_path).await.unwrap();
        let (mut reader, mut writer) = tokio::io::split(stream);
        write_message(
            &mut writer,
            &ClientMessage::Register {
                client_type: "test".into(),
                mode: ClientMode::Stdio,
                capabilities: ClientCapabilities::default(),
            },
        )
        .await
        .unwrap();
        let _: ServerMessage = read_message(&mut reader).await.unwrap();
        write_message(
            &mut writer,
            &ClientMessage::Acp {
                payload: r#"{"jsonrpc":"2.0","method":"test/ping","id":42}"#.into(),
            },
        )
        .await
        .unwrap();
        let forwarded = handle.acp_rx.recv().await.unwrap();
        let json: serde_json::Value = serde_json::from_str(&forwarded).unwrap();
        let namespaced_id = json["id"].as_str().unwrap().to_string();
        assert!(handle.agent_busy.load(Ordering::Relaxed));
        let response = format!(
            r#"{{"jsonrpc":"2.0","result":{{"ok":true}},"id":"{}"}}"#,
            namespaced_id
        );
        handle.response_tx.send(response).unwrap();
        let client_resp: ServerMessage = read_message(&mut reader).await.unwrap();
        assert!(matches!(client_resp, ServerMessage::Acp { .. }));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !handle.agent_busy.load(Ordering::Relaxed),
            "agent_busy should be false after response is routed"
        );
        handle.cancel.cancel();
    }
    #[tokio::test]
    async fn agent_busy_tracks_multiple_pending_requests() {
        let temp = TempDir::new().unwrap();
        let sock_path = temp.path().join("busy_multi.sock");
        let mut handle = spawn_leader_server(sock_path.clone()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stream = LeaderStream::connect(&sock_path).await.unwrap();
        let (mut reader, mut writer) = tokio::io::split(stream);
        write_message(
            &mut writer,
            &ClientMessage::Register {
                client_type: "test".into(),
                mode: ClientMode::Stdio,
                capabilities: ClientCapabilities::default(),
            },
        )
        .await
        .unwrap();
        let _: ServerMessage = read_message(&mut reader).await.unwrap();
        write_message(
            &mut writer,
            &ClientMessage::Acp {
                payload: r#"{"jsonrpc":"2.0","method":"test/a","id":1}"#.into(),
            },
        )
        .await
        .unwrap();
        write_message(
            &mut writer,
            &ClientMessage::Acp {
                payload: r#"{"jsonrpc":"2.0","method":"test/b","id":2}"#.into(),
            },
        )
        .await
        .unwrap();
        let fwd1 = handle.acp_rx.recv().await.unwrap();
        let fwd2 = handle.acp_rx.recv().await.unwrap();
        let id1 = serde_json::from_str::<serde_json::Value>(&fwd1).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let id2 = serde_json::from_str::<serde_json::Value>(&fwd2).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(handle.agent_busy.load(Ordering::Relaxed));
        handle
            .response_tx
            .send(format!(
                r#"{{"jsonrpc":"2.0","result":{{}},"id":"{}"}}"#,
                id1
            ))
            .unwrap();
        let _: ServerMessage = read_message(&mut reader).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            handle.agent_busy.load(Ordering::Relaxed),
            "agent_busy should still be true with one request pending"
        );
        handle
            .response_tx
            .send(format!(
                r#"{{"jsonrpc":"2.0","result":{{}},"id":"{}"}}"#,
                id2
            ))
            .unwrap();
        let _: ServerMessage = read_message(&mut reader).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !handle.agent_busy.load(Ordering::Relaxed),
            "agent_busy should be false after all responses received"
        );
        handle.cancel.cancel();
    }
    #[tokio::test]
    async fn agent_busy_clears_when_client_disconnects_mid_request() {
        let temp = TempDir::new().unwrap();
        let sock_path = temp.path().join("busy_disconnect.sock");
        let (acp_tx, mut acp_rx) = mpsc::unbounded_channel();
        let (response_tx, response_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let client_count = Arc::new(AtomicUsize::new(0));
        let agent_busy = Arc::new(AtomicBool::new(false));
        let control_state = default_test_control_state(&sock_path);
        let sock_clone = sock_path.clone();
        let cancel_clone = cancel.clone();
        let count_clone = client_count.clone();
        let busy_clone = agent_busy.clone();
        tokio::spawn(async move {
            let _ = run_leader_server(
                sock_clone,
                acp_tx,
                response_rx,
                cancel_clone,
                true,
                count_clone,
                busy_clone,
                AgentActivity::default(),
                watch::channel(true).1,
                watch::channel(false).0,
                watch::channel(super::super::protocol::ShutdownReason::Manual).0,
                None,
                control_state,
            )
            .await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let namespaced_id = {
            let stream = LeaderStream::connect(&sock_path).await.unwrap();
            let (mut reader, mut writer) = tokio::io::split(stream);
            write_message(
                &mut writer,
                &ClientMessage::Register {
                    client_type: "test".into(),
                    mode: ClientMode::Stdio,
                    capabilities: ClientCapabilities::default(),
                },
            )
            .await
            .unwrap();
            let _: ServerMessage = read_message(&mut reader).await.unwrap();
            write_message(
                &mut writer,
                &ClientMessage::Acp {
                    payload: r#"{"jsonrpc":"2.0","method":"test/slow","id":1}"#.into(),
                },
            )
            .await
            .unwrap();
            let forwarded = acp_rx.recv().await.unwrap();
            let json: serde_json::Value = serde_json::from_str(&forwarded).unwrap();
            let id = json["id"].as_str().unwrap().to_string();
            assert!(
                agent_busy.load(Ordering::Relaxed),
                "should be busy after request"
            );
            write_message(&mut writer, &ClientMessage::Disconnect)
                .await
                .unwrap();
            id
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            agent_busy.load(Ordering::Relaxed),
            "agent_busy should still be true after client disconnect (request still pending)"
        );
        response_tx
            .send(format!(
                r#"{{"jsonrpc":"2.0","result":{{"done":true}},"id":"{}"}}"#,
                namespaced_id
            ))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !agent_busy.load(Ordering::Relaxed),
            "agent_busy should be false after response arrives (even though client disconnected)"
        );
        cancel.cancel();
    }
    /// Regression: bounded(256) client channel + try_send silently dropped
    /// notifications during session replay bursts. Unbounded channel fixes this.
    #[tokio::test]
    async fn high_throughput_replay_no_drops() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx, mut acp_rx) =
            setup_persistent_server_with_agent(&temp).await;
        let (mut reader, mut writer) = connect_and_register(&sock_path, "grok-tui").await;
        let load_req = r#"{"jsonrpc":"2.0","method":"session/load","id":1,"params":{"session_id":"sess_replay"}}"#;
        write_message(
            &mut writer,
            &ClientMessage::Acp {
                payload: load_req.into(),
            },
        )
        .await
        .unwrap();
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        const REPLAY_COUNT: usize = 500;
        for i in 0..REPLAY_COUNT {
            let notification = format!(
                r#"{{"jsonrpc":"2.0","method":"session/notification","params":{{"session_id":"sess_replay","updates":[{{"type":"message_start","message_id":"msg_{i}"}}]}}}}"#,
            );
            response_tx.send(notification).unwrap();
        }
        let mut received = 0usize;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline - tokio::time::Instant::now();
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, read_message::<_, ServerMessage>(&mut reader))
                .await
            {
                Ok(Ok(ServerMessage::Acp { .. })) => {
                    received += 1;
                    if received == REPLAY_COUNT {
                        break;
                    }
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => break,
            }
        }
        assert_eq!(
            received, REPLAY_COUNT,
            "All {REPLAY_COUNT} replay notifications must arrive, got {received}"
        );
        cancel.cancel();
    }
    /// When a client disconnects after interacting with a session, the server
    /// sends an `x.ai/internal/evict_sessions` notification through acp_tx
    /// so the agent can release session memory.
    #[tokio::test]
    async fn evict_sessions_notification_on_disconnect() {
        let temp = TempDir::new().unwrap();
        let sock_path = temp.path().join("test.sock");
        let (acp_tx, mut acp_rx) = mpsc::unbounded_channel();
        let (_response_tx, response_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let control_state = default_test_control_state(&sock_path);
        let sock_clone = sock_path.clone();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            let _ = run_leader_server(
                sock_clone,
                acp_tx,
                response_rx,
                cancel_clone,
                true,
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicBool::new(false)),
                AgentActivity::default(),
                watch::channel(true).1,
                watch::channel(false).0,
                watch::channel(super::super::protocol::ShutdownReason::Manual).0,
                None,
                control_state,
            )
            .await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (mut _reader, mut writer) = connect_and_register(&sock_path, "test-client").await;
        let msg = r#"{"jsonrpc":"2.0","method":"session/load","id":1,"params":{"sessionId":"sess-evict-test"}}"#;
        write_message(
            &mut writer,
            &ClientMessage::Acp {
                payload: msg.into(),
            },
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = acp_rx.recv().await;
        write_message(&mut writer, &ClientMessage::Disconnect)
            .await
            .unwrap();
        drop(_reader);
        drop(writer);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let eviction_msg = tokio::time::timeout(Duration::from_secs(1), acp_rx.recv())
            .await
            .expect("should receive eviction notification")
            .expect("channel should not be closed");
        let json: serde_json::Value =
            serde_json::from_str(&eviction_msg).expect("should be valid JSON");
        assert_eq!(json["method"], "x.ai/internal/evict_sessions");
        let session_ids = json["params"]["sessionIds"]
            .as_array()
            .expect("sessionIds should be an array");
        assert!(
            session_ids
                .iter()
                .any(|v| v.as_str() == Some("sess-evict-test")),
            "eviction should include the session we interacted with, got: {session_ids:?}"
        );
        cancel.cancel();
    }
    /// When a client disconnects without interacting with any sessions,
    /// no eviction notification should be sent.
    #[tokio::test]
    async fn no_eviction_when_client_has_no_sessions() {
        let temp = TempDir::new().unwrap();
        let sock_path = temp.path().join("test.sock");
        let (acp_tx, mut acp_rx) = mpsc::unbounded_channel();
        let (_response_tx, response_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let control_state = default_test_control_state(&sock_path);
        let sock_clone = sock_path.clone();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            let _ = run_leader_server(
                sock_clone,
                acp_tx,
                response_rx,
                cancel_clone,
                true,
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicBool::new(false)),
                AgentActivity::default(),
                watch::channel(true).1,
                watch::channel(false).0,
                watch::channel(super::super::protocol::ShutdownReason::Manual).0,
                None,
                control_state,
            )
            .await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (mut _reader, mut writer) = connect_and_register(&sock_path, "idle-client").await;
        write_message(&mut writer, &ClientMessage::Disconnect)
            .await
            .unwrap();
        drop(_reader);
        drop(writer);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            acp_rx.try_recv().is_err(),
            "no eviction notification should be sent for clients with no sessions"
        );
        cancel.cancel();
    }
    /// Read the next `ServerMessage::Acp` payload for a client, ignoring other
    /// server messages, with a short deadline. Returns `None` on timeout.
    async fn next_acp_payload(reader: &mut tokio::io::ReadHalf<LeaderStream>) -> Option<String> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
        loop {
            let remaining = deadline - tokio::time::Instant::now();
            if remaining.is_zero() {
                return None;
            }
            match tokio::time::timeout(remaining, read_message::<_, ServerMessage>(reader)).await {
                Ok(Ok(ServerMessage::Acp { payload })) => return Some(payload),
                Ok(Ok(_)) => continue,
                Ok(Err(_)) | Err(_) => return None,
            }
        }
    }
    /// Drain up to a few ACP payloads looking for one containing `needle`.
    /// Returns it if found within the window, else `None` (so a "must NOT
    /// receive" assertion can use `.is_none()`).
    async fn next_acp_payload_matching(
        reader: &mut tokio::io::ReadHalf<LeaderStream>,
        needle: &str,
    ) -> Option<String> {
        for _ in 0..8 {
            match next_acp_payload(reader).await {
                Some(p) if p.contains(needle) => return Some(p),
                Some(_) => continue,
                None => return None,
            }
        }
        None
    }
    async fn load_session(writer: &mut tokio::io::WriteHalf<LeaderStream>, session_id: &str) {
        let msg = format!(
            r#"{{"jsonrpc":"2.0","method":"session/load","id":1,"params":{{"sessionId":"{session_id}"}}}}"#
        );
        write_message(writer, &ClientMessage::Acp { payload: msg })
            .await
            .unwrap();
    }
    /// Regression (live-before-replay race): a live `session/notification` that
    /// arrives WHILE a viewer's `session/load` is in flight must be BUFFERED —
    /// not delivered early (which would bump the client's eventId highwater and
    /// make the subsequent lower-eventId replay get deduped away) — and then
    /// flushed, in order, AFTER the load response.
    #[tokio::test]
    async fn live_broadcast_during_load_is_buffered_then_flushed_after_response() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx, mut acp_rx) =
            setup_persistent_server_with_agent(&temp).await;
        let (mut reader, mut writer) = connect_and_register(&sock_path, "viewer").await;
        load_session(&mut writer, "sess-buf").await;
        let forwarded = tokio::time::timeout(Duration::from_secs(1), acp_rx.recv())
            .await
            .expect("timed out waiting for forwarded load")
            .expect("agent channel closed");
        let load_id = serde_json::from_str::<serde_json::Value>(&forwarded)
            .unwrap()
            .get("id")
            .cloned()
            .unwrap();
        let live = r#"{"jsonrpc":"2.0","method":"session/notification","params":{"sessionId":"sess-buf","updates":[{"type":"message_start","message_id":"live1"}]}}"#;
        response_tx.send(live.to_string()).unwrap();
        let early = tokio::time::timeout(
            Duration::from_millis(250),
            read_message::<_, ServerMessage>(&mut reader),
        )
        .await;
        assert!(
            early.is_err(),
            "live broadcast must be buffered until the load response, got {early:?}"
        );
        let response = serde_json::json!(
            { "jsonrpc" : "2.0", "id" : load_id, "result" : { "models" : [] }, }
        );
        response_tx.send(response.to_string()).unwrap();
        let first = next_acp_payload(&mut reader).await;
        assert!(
            first.as_deref().is_some_and(|p| p.contains("\"models\"")),
            "first message after load must be the load response, got {first:?}"
        );
        let second = next_acp_payload(&mut reader).await;
        assert!(
            second.as_deref().is_some_and(|p| p.contains("live1")),
            "buffered live notif must arrive (in order) after the load response, got {second:?}"
        );
        cancel.cancel();
    }
    /// Two clients load the same session; a `session/notification` (no `id`)
    /// must reach BOTH (broadcast), while a reverse-request (`id` + `method`)
    /// reaches ONLY the driver. The second client's `session/load` must not
    /// black out the first (join-not-steal).
    #[tokio::test]
    async fn two_clients_one_session_broadcast_and_driver() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx, mut acp_rx) =
            setup_persistent_server_with_agent(&temp).await;
        let (mut reader_a, mut writer_a) = connect_and_register(&sock_path, "client-a").await;
        load_session(&mut writer_a, "sess-multi").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let (mut reader_b, mut writer_b) = connect_and_register(&sock_path, "client-b").await;
        load_session(&mut writer_b, "sess-multi").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_b).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let notif = r#"{"jsonrpc":"2.0","method":"session/notification","params":{"sessionId":"sess-multi","updates":[{"type":"message_start","message_id":"m1"}]}}"#;
        response_tx.send(notif.to_string()).unwrap();
        let got_a = next_acp_payload(&mut reader_a).await;
        let got_b = next_acp_payload(&mut reader_b).await;
        assert!(
            got_a.as_deref().is_some_and(|p| p.contains("m1")),
            "client A must receive the broadcast notification, got {got_a:?}"
        );
        assert!(
            got_b.as_deref().is_some_and(|p| p.contains("m1")),
            "client B must receive the broadcast notification (no blackout), got {got_b:?}"
        );
        let req = r#"{"jsonrpc":"2.0","id":42,"method":"fs/read_text_file","params":{"sessionId":"sess-multi","path":"/tmp/x"}}"#;
        response_tx.send(req.to_string()).unwrap();
        let req_a = next_acp_payload(&mut reader_a).await;
        let req_b = next_acp_payload(&mut reader_b).await;
        assert!(
            req_a
                .as_deref()
                .is_some_and(|p| p.contains("read_text_file")),
            "driver A must receive the reverse-request, got {req_a:?}"
        );
        assert!(
            req_b.is_none(),
            "non-driver B must NOT receive the reverse-request, got {req_b:?}"
        );
        cancel.cancel();
    }
    /// A `x.ai/scheduled_task_inject_prompt` (cron `/loop` fire) must be routed
    /// to the SINGLE session driver, not fanned out to every subscriber. If it
    /// broadcast, each attached dashboard would enqueue + try to drive the same
    /// cron turn (phantom `#N` queue rows, competing drivers, stuck turns). The
    /// other clients render the resulting turn from the broadcast deltas.
    #[tokio::test]
    async fn scheduled_task_inject_prompt_routes_to_driver_only() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx, mut acp_rx) =
            setup_persistent_server_with_agent(&temp).await;
        let (mut reader_a, mut writer_a) = connect_and_register(&sock_path, "client-a").await;
        load_session(&mut writer_a, "sess-cron").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let (mut reader_b, mut writer_b) = connect_and_register(&sock_path, "client-b").await;
        load_session(&mut writer_b, "sess-cron").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_b).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let inject = r#"{"method":"_x.ai/scheduled_task_inject_prompt","params":{"method":"x.ai/scheduled_task_inject_prompt","params":{"sessionId":"sess-cron","taskId":"task-1","prompt":"echo hello","humanSchedule":"every 1m"}}}"#;
        response_tx.send(inject.to_string()).unwrap();
        let got_a = next_acp_payload(&mut reader_a).await;
        let got_b = next_acp_payload(&mut reader_b).await;
        assert!(
            got_a
                .as_deref()
                .is_some_and(|p| p.contains("scheduled_task_inject_prompt")),
            "driver A must receive the cron inject_prompt, got {got_a:?}"
        );
        assert!(
            got_b.is_none(),
            "non-driver B must NOT receive the cron inject_prompt, got {got_b:?}"
        );
        cancel.cancel();
    }
    /// A blocking interaction reverse-request (permission / `ask_user_question` /
    /// plan-approval) is SHARED: broadcast to every subscriber so any client can
    /// render + answer the modal. Contrast
    /// with `two_clients_one_session_broadcast_and_driver`, where an ordinary
    /// reverse-request reaches the driver only.
    #[tokio::test]
    async fn interaction_request_broadcasts_to_all_subscribers() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx, mut acp_rx) =
            setup_persistent_server_with_agent(&temp).await;
        let (mut reader_a, mut writer_a) = connect_and_register(&sock_path, "client-a").await;
        load_session(&mut writer_a, "sess-int").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let (mut reader_b, mut writer_b) = connect_and_register(&sock_path, "client-b").await;
        load_session(&mut writer_b, "sess-int").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_b).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let req = r#"{"jsonrpc":"2.0","id":501,"method":"_x.ai/ask_user_question","params":{"method":"x.ai/ask_user_question","params":{"sessionId":"sess-int","toolCallId":"tc-q","questions":[]}}}"#;
        response_tx.send(req.to_string()).unwrap();
        let got_a = next_acp_payload_matching(&mut reader_a, "ask_user_question").await;
        let got_b = next_acp_payload_matching(&mut reader_b, "ask_user_question").await;
        assert!(
            got_a.is_some(),
            "driver A must receive the shared interaction"
        );
        assert!(
            got_b.is_some(),
            "subscriber B must ALSO receive the shared interaction (not driver-only)"
        );
        cancel.cancel();
    }
    /// A client that attaches WHILE an interaction is pending must render it too:
    /// the leader caches the issued interaction and replays it to the new
    /// subscriber after its `session/load` completes.
    #[tokio::test]
    async fn pending_interaction_replayed_to_late_joiner() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx, mut acp_rx) =
            setup_persistent_server_with_agent(&temp).await;
        let (mut reader_a, mut writer_a) = connect_and_register(&sock_path, "client-a").await;
        load_session(&mut writer_a, "sess-int").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let req = r#"{"jsonrpc":"2.0","id":601,"method":"_x.ai/ask_user_question","params":{"method":"x.ai/ask_user_question","params":{"sessionId":"sess-int","toolCallId":"tc-late","questions":[]}}}"#;
        response_tx.send(req.to_string()).unwrap();
        let _ = next_acp_payload_matching(&mut reader_a, "ask_user_question").await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let (mut reader_b, mut writer_b) = connect_and_register(&sock_path, "client-b").await;
        load_session(&mut writer_b, "sess-int").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let replayed = next_acp_payload_matching(&mut reader_b, "ask_user_question").await;
        assert!(
            replayed.is_some(),
            "a late-joiner must receive the replayed still-pending interaction"
        );
        cancel.cancel();
    }
    /// Like [`connect_and_register`] but also returns the server-assigned
    /// `ClientId` (needed to address targeted replay payloads at the client).
    async fn connect_register_get_id(
        sock_path: &std::path::Path,
        client_type: &str,
    ) -> (
        tokio::io::ReadHalf<LeaderStream>,
        tokio::io::WriteHalf<LeaderStream>,
        ClientId,
    ) {
        let stream = LeaderStream::connect(sock_path).await.unwrap();
        let (mut reader, mut writer) = tokio::io::split(stream);
        write_message(
            &mut writer,
            &ClientMessage::Register {
                client_type: client_type.into(),
                mode: ClientMode::Stdio,
                capabilities: ClientCapabilities::default(),
            },
        )
        .await
        .unwrap();
        let msg: ServerMessage = read_message(&mut reader).await.unwrap();
        let ServerMessage::Registered { client_id, .. } = msg else {
            panic!("expected Registered, got {msg:?}");
        };
        (reader, writer, ClientId(client_id))
    }
    /// A client that reattaches AFTER a subagent spawned is backfilled into
    /// the child route when its parent `session/load` response lands: the
    /// parent→child index survives the disconnect eviction (which only
    /// empties subscriber sets), so live child updates resume without any
    /// replayed spawn line. Driver inheritance is pinned too: a driver-only
    /// child reverse-request must reach the reattached client.
    #[tokio::test]
    async fn reattached_client_backfilled_into_child_routes() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx, mut acp_rx) =
            setup_persistent_server_with_agent(&temp).await;
        let (mut reader_a, mut writer_a) = connect_and_register(&sock_path, "client-a").await;
        load_session(&mut writer_a, "sess-sub").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let spawned_live = r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"sess-sub","update":{"sessionUpdate":"subagent_spawned","child_session_id":"child-sub"}}}"#;
        response_tx.send(spawned_live.to_string()).unwrap();
        assert!(
            next_acp_payload_matching(&mut reader_a, "subagent_spawned")
                .await
                .is_some(),
            "sanity: A receives the live spawn"
        );
        drop(reader_a);
        drop(writer_a);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (mut reader_a2, mut writer_a2) = connect_and_register(&sock_path, "client-a").await;
        load_session(&mut writer_a2, "sess-sub").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a2).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let child_live = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"child-sub","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"CHILD_LIVE_DELTA"}}}}"#;
        response_tx.send(child_live.to_string()).unwrap();
        assert!(
            next_acp_payload_matching(&mut reader_a2, "CHILD_LIVE_DELTA")
                .await
                .is_some(),
            "live child updates must reach the reattached client via backfill"
        );
        let child_reverse = r#"{"jsonrpc":"2.0","id":777,"method":"x.ai/child_thing","params":{"sessionId":"child-sub"}}"#;
        response_tx.send(child_reverse.to_string()).unwrap();
        assert!(
            next_acp_payload_matching(&mut reader_a2, "child_thing")
                .await
                .is_some(),
            "child reverse-requests must reach the backfilled driver"
        );
        cancel.cancel();
    }
    /// A loading client receives the child route from the targeted REPLAYED
    /// `subagent_spawned` alone (fresh-leader relaunch: no live spawn ever
    /// crossed this server instance, the index is empty, only replay lines
    /// describe the subagent).
    #[tokio::test]
    async fn replayed_spawn_registers_child_route_for_loading_client() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx, mut acp_rx) =
            setup_persistent_server_with_agent(&temp).await;
        let (mut reader_a, mut writer_a, a_id) =
            connect_register_get_id(&sock_path, "client-a").await;
        load_session(&mut writer_a, "sess-fresh").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let spawned_replay = format!(
            r#"{{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{{"sessionId":"sess-fresh","_meta":{{"isReplay":true,"x.ai/leaderClientId":{}}},"update":{{"sessionUpdate":"subagent_spawned","child_session_id":"child-fresh"}}}}}}"#,
            a_id.0
        );
        response_tx.send(spawned_replay).unwrap();
        assert!(
            next_acp_payload_matching(&mut reader_a, "subagent_spawned")
                .await
                .is_some(),
            "the replayed spawn row reaches the loading client"
        );
        let child_live = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"child-fresh","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"CHILD_FRESH_DELTA"}}}}"#;
        response_tx.send(child_live.to_string()).unwrap();
        assert!(
            next_acp_payload_matching(&mut reader_a, "CHILD_FRESH_DELTA")
                .await
                .is_some(),
            "the replayed spawn must register the live child route"
        );
        cancel.cancel();
    }
    /// A client that attaches to the parent while another client already holds
    /// a live child route is backfilled into that route (child sets are
    /// spawn-time snapshots; joining the parent must join its descendants).
    #[tokio::test]
    async fn late_attacher_backfilled_into_existing_child_routes() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx, mut acp_rx) =
            setup_persistent_server_with_agent(&temp).await;
        let (mut reader_a, mut writer_a) = connect_and_register(&sock_path, "client-a").await;
        load_session(&mut writer_a, "sess-sub2").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let spawned_live = r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"sess-sub2","update":{"sessionUpdate":"subagent_spawned","child_session_id":"child-sub2"}}}"#;
        response_tx.send(spawned_live.to_string()).unwrap();
        let _ = next_acp_payload_matching(&mut reader_a, "subagent_spawned").await;
        let (mut reader_b, mut writer_b) = connect_and_register(&sock_path, "client-b").await;
        load_session(&mut writer_b, "sess-sub2").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_b).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let child_live = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"child-sub2","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"CHILD_LIVE_DELTA2"}}}}"#;
        response_tx.send(child_live.to_string()).unwrap();
        assert!(
            next_acp_payload_matching(&mut reader_a, "CHILD_LIVE_DELTA2")
                .await
                .is_some(),
            "A (in the spawn-time snapshot) still receives child updates"
        );
        assert!(
            next_acp_payload_matching(&mut reader_b, "CHILD_LIVE_DELTA2")
                .await
                .is_some(),
            "the late attacher must be backfilled into the child route"
        );
        cancel.cancel();
    }
    /// A replayed `subagent_finished` unsubscribes ONLY its target client:
    /// another client's live child route must survive one client's history
    /// replay (full teardown is reserved for the LIVE finish).
    #[tokio::test]
    async fn replayed_finished_does_not_tear_down_live_child_route() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx, mut acp_rx) =
            setup_persistent_server_with_agent(&temp).await;
        let (mut reader_a, mut writer_a) = connect_and_register(&sock_path, "client-a").await;
        load_session(&mut writer_a, "sess-tear").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let spawned_live = r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"sess-tear","update":{"sessionUpdate":"subagent_spawned","child_session_id":"child-tear"}}}"#;
        response_tx.send(spawned_live.to_string()).unwrap();
        let _ = next_acp_payload_matching(&mut reader_a, "subagent_spawned").await;
        let (mut reader_b, mut writer_b, b_id) =
            connect_register_get_id(&sock_path, "client-b").await;
        load_session(&mut writer_b, "sess-tear").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_b).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let finished_replay = format!(
            r#"{{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{{"sessionId":"sess-tear","_meta":{{"isReplay":true,"x.ai/leaderClientId":{}}},"update":{{"sessionUpdate":"subagent_finished","child_session_id":"child-tear"}}}}}}"#,
            b_id.0
        );
        response_tx.send(finished_replay).unwrap();
        let _ = next_acp_payload_matching(&mut reader_b, "subagent_finished").await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let child_live = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"child-tear","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"CHILD_TEAR_DELTA"}}}}"#;
        response_tx.send(child_live.to_string()).unwrap();
        assert!(
            next_acp_payload_matching(&mut reader_a, "CHILD_TEAR_DELTA")
                .await
                .is_some(),
            "A's live child route must survive B's replayed finished"
        );
        assert!(
            next_acp_payload_matching(&mut reader_b, "CHILD_TEAR_DELTA")
                .await
                .is_none(),
            "B was unsubscribed by ITS replayed finished"
        );
        cancel.cancel();
    }
    /// Backfill walks the index depth-first: a nested child (spawned under a
    /// CHILD session) is also joined when a client attaches to the root
    /// parent.
    #[tokio::test]
    async fn backfill_covers_nested_children() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx, mut acp_rx) =
            setup_persistent_server_with_agent(&temp).await;
        let (mut reader_a, mut writer_a) = connect_and_register(&sock_path, "client-a").await;
        load_session(&mut writer_a, "sess-nest").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let spawned_child = r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"sess-nest","update":{"sessionUpdate":"subagent_spawned","child_session_id":"child-nest"}}}"#;
        response_tx.send(spawned_child.to_string()).unwrap();
        let _ = next_acp_payload_matching(&mut reader_a, "subagent_spawned").await;
        let spawned_grandchild = r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"child-nest","update":{"sessionUpdate":"subagent_spawned","child_session_id":"grandchild-nest"}}}"#;
        response_tx.send(spawned_grandchild.to_string()).unwrap();
        let _ = next_acp_payload_matching(&mut reader_a, "grandchild-nest").await;
        drop(reader_a);
        drop(writer_a);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (mut reader_a2, mut writer_a2) = connect_and_register(&sock_path, "client-a").await;
        load_session(&mut writer_a2, "sess-nest").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a2).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let grandchild_live = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"grandchild-nest","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"GRANDCHILD_DELTA"}}}}"#;
        response_tx.send(grandchild_live.to_string()).unwrap();
        assert!(
            next_acp_payload_matching(&mut reader_a2, "GRANDCHILD_DELTA")
                .await
                .is_some(),
            "backfill must subscribe the client to nested descendants"
        );
        cancel.cancel();
    }
    /// Re-parenting on an INTERMEDIATE finish: root → A → B (both live). A
    /// finishes LIVE while B keeps running. A new client loading the ROOT must
    /// still be backfilled into B's live route — `prune_child_route` promotes B
    /// onto A's parent so the forward-only root walk reaches it. Without
    /// re-parenting the root→A edge is gone and B's subtree is orphaned.
    #[tokio::test]
    async fn intermediate_finish_reparents_live_grandchild_for_root_backfill() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx, mut acp_rx) =
            setup_persistent_server_with_agent(&temp).await;
        let (mut reader_a, mut writer_a) = connect_and_register(&sock_path, "client-a").await;
        load_session(&mut writer_a, "sess-rep").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let spawned_a = r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"sess-rep","update":{"sessionUpdate":"subagent_spawned","child_session_id":"child-a"}}}"#;
        response_tx.send(spawned_a.to_string()).unwrap();
        let _ = next_acp_payload_matching(&mut reader_a, "child-a").await;
        let spawned_b = r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"child-a","update":{"sessionUpdate":"subagent_spawned","child_session_id":"grandchild-b"}}}"#;
        response_tx.send(spawned_b.to_string()).unwrap();
        let _ = next_acp_payload_matching(&mut reader_a, "grandchild-b").await;
        let finished_a = r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"sess-rep","update":{"sessionUpdate":"subagent_finished","child_session_id":"child-a"}}}"#;
        response_tx.send(finished_a.to_string()).unwrap();
        let _ = next_acp_payload_matching(&mut reader_a, "subagent_finished").await;
        drop(reader_a);
        drop(writer_a);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (mut reader_a2, mut writer_a2) = connect_and_register(&sock_path, "client-a").await;
        load_session(&mut writer_a2, "sess-rep").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a2).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let grandchild_live = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"grandchild-b","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"LIVE_GRANDCHILD_AFTER_A_FINISH"}}}}"#;
        response_tx.send(grandchild_live.to_string()).unwrap();
        assert!(
            next_acp_payload_matching(&mut reader_a2, "LIVE_GRANDCHILD_AFTER_A_FINISH")
                .await
                .is_some(),
            "an intermediate finish must re-parent the live grandchild so root backfill still reaches it"
        );
        cancel.cancel();
    }
    /// The LIVE `subagent_finished` still tears the route down globally and
    /// prunes the index: after it, a reattaching client is NOT backfilled
    /// into the dead child (no leaked routes for finished subagents).
    #[tokio::test]
    async fn live_finished_prunes_index_so_reattach_skips_dead_child() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx, mut acp_rx) =
            setup_persistent_server_with_agent(&temp).await;
        let (mut reader_a, mut writer_a) = connect_and_register(&sock_path, "client-a").await;
        load_session(&mut writer_a, "sess-dead").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let spawned_live = r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"sess-dead","update":{"sessionUpdate":"subagent_spawned","child_session_id":"child-dead"}}}"#;
        response_tx.send(spawned_live.to_string()).unwrap();
        let _ = next_acp_payload_matching(&mut reader_a, "subagent_spawned").await;
        let finished_live = r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"sess-dead","update":{"sessionUpdate":"subagent_finished","child_session_id":"child-dead"}}}"#;
        response_tx.send(finished_live.to_string()).unwrap();
        let _ = next_acp_payload_matching(&mut reader_a, "subagent_finished").await;
        drop(reader_a);
        drop(writer_a);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (mut reader_a2, mut writer_a2) = connect_and_register(&sock_path, "client-a").await;
        load_session(&mut writer_a2, "sess-dead").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a2).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let child_live = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"child-dead","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"DEAD_CHILD_DELTA"}}}}"#;
        response_tx.send(child_live.to_string()).unwrap();
        assert!(
            next_acp_payload_matching(&mut reader_a2, "DEAD_CHILD_DELTA")
                .await
                .is_none(),
            "a finished child's route must not be resurrected by reattach"
        );
        cancel.cancel();
    }
    /// Symmetric twin of `live_finished_prunes_index_so_reattach_skips_dead_child`
    /// for the no-subscribers case: the parent goes fully detached (every
    /// client disconnects, the index edge survives), THEN a live
    /// `subagent_finished` arrives. It is relay-classified (no subscribers) and
    /// dropped — but it must still prune the index edge, so a reattaching
    /// client's `session/load` backfill does not resurrect the dead child.
    #[tokio::test]
    async fn detached_live_finished_prunes_index_so_reattach_skips_dead_child() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx, mut acp_rx) =
            setup_persistent_server_with_agent(&temp).await;
        let (mut reader_a, mut writer_a) = connect_and_register(&sock_path, "client-a").await;
        load_session(&mut writer_a, "sess-detach").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let spawned_live = r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"sess-detach","update":{"sessionUpdate":"subagent_spawned","child_session_id":"child-detach"}}}"#;
        response_tx.send(spawned_live.to_string()).unwrap();
        let _ = next_acp_payload_matching(&mut reader_a, "subagent_spawned").await;
        drop(reader_a);
        drop(writer_a);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let finished_live = r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"sess-detach","update":{"sessionUpdate":"subagent_finished","child_session_id":"child-detach"}}}"#;
        response_tx.send(finished_live.to_string()).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let (mut reader_a2, mut writer_a2) = connect_and_register(&sock_path, "client-a").await;
        load_session(&mut writer_a2, "sess-detach").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a2).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let child_live = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"child-detach","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"DETACHED_DEAD_CHILD_DELTA"}}}}"#;
        response_tx.send(child_live.to_string()).unwrap();
        assert!(
            next_acp_payload_matching(&mut reader_a2, "DETACHED_DEAD_CHILD_DELTA")
                .await
                .is_none(),
            "a detached live finish must prune the edge — the dead child's route \
             must not be resurrected by reattach backfill"
        );
        cancel.cancel();
    }
    /// A loader disconnecting between a dead child's replayed spawn and
    /// replayed finish must not leak the index edge: the orphan-drop arm still
    /// prunes when nothing holds the route, so a later attacher is not
    /// backfilled into the dead child.
    #[tokio::test]
    async fn mid_burst_disconnect_still_prunes_dead_child_route() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx, mut acp_rx) =
            setup_persistent_server_with_agent(&temp).await;
        let (mut reader_a, mut writer_a, a_id) =
            connect_register_get_id(&sock_path, "client-a").await;
        load_session(&mut writer_a, "sess-leak").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let spawned_replay = format!(
            r#"{{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{{"sessionId":"sess-leak","_meta":{{"isReplay":true,"x.ai/leaderClientId":{}}},"update":{{"sessionUpdate":"subagent_spawned","child_session_id":"child-leak"}}}}}}"#,
            a_id.0
        );
        response_tx.send(spawned_replay).unwrap();
        let _ = next_acp_payload_matching(&mut reader_a, "subagent_spawned").await;
        drop(reader_a);
        drop(writer_a);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let finished_replay = format!(
            r#"{{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{{"sessionId":"sess-leak","_meta":{{"isReplay":true,"x.ai/leaderClientId":{}}},"update":{{"sessionUpdate":"subagent_finished","child_session_id":"child-leak"}}}}}}"#,
            a_id.0
        );
        response_tx.send(finished_replay).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let (mut reader_a2, mut writer_a2) = connect_and_register(&sock_path, "client-a").await;
        load_session(&mut writer_a2, "sess-leak").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a2).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let child_live = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"child-leak","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"LEAKED_CHILD_DELTA"}}}}"#;
        response_tx.send(child_live.to_string()).unwrap();
        assert!(
            next_acp_payload_matching(&mut reader_a2, "LEAKED_CHILD_DELTA")
                .await
                .is_none(),
            "an orphaned replayed finish must prune the edge — the dead child's \
             route must not be resurrected by reattach backfill"
        );
        cancel.cancel();
    }
    /// An ORPHANED replayed finish (its target already vanished) must leave a
    /// route other clients hold untouched — the orphan branch prunes only
    /// when nothing holds the route. An always-prune mutation of that guard
    /// would let one dead client's stale replay burst tear down A's live
    /// route.
    #[tokio::test]
    async fn orphaned_replayed_finished_leaves_held_route_untouched() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx, mut acp_rx) =
            setup_persistent_server_with_agent(&temp).await;
        let (mut reader_a, mut writer_a) = connect_and_register(&sock_path, "client-a").await;
        load_session(&mut writer_a, "sess-hold").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let spawned_live = r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"sess-hold","update":{"sessionUpdate":"subagent_spawned","child_session_id":"child-hold"}}}"#;
        response_tx.send(spawned_live.to_string()).unwrap();
        let _ = next_acp_payload_matching(&mut reader_a, "subagent_spawned").await;
        let (reader_b, writer_b, b_id) = connect_register_get_id(&sock_path, "client-b").await;
        drop(reader_b);
        drop(writer_b);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let finished_replay = format!(
            r#"{{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{{"sessionId":"sess-hold","_meta":{{"isReplay":true,"x.ai/leaderClientId":{}}},"update":{{"sessionUpdate":"subagent_finished","child_session_id":"child-hold"}}}}}}"#,
            b_id.0
        );
        response_tx.send(finished_replay).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let child_live = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"child-hold","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"HELD_DELTA"}}}}"#;
        response_tx.send(child_live.to_string()).unwrap();
        assert!(
            next_acp_payload_matching(&mut reader_a, "HELD_DELTA")
                .await
                .is_some(),
            "an orphaned replayed finish must not prune a route A still holds"
        );
        cancel.cancel();
    }
    /// A replayed spawn UNIONS the loading client into an existing live route:
    /// a regression to the live arm's snapshot-replace would tear down the
    /// holder's route on someone else's history replay (the symmetric twin of
    /// `replayed_finished_does_not_tear_down_live_child_route`).
    #[tokio::test]
    async fn replayed_spawn_unions_into_existing_live_route() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx, mut acp_rx) =
            setup_persistent_server_with_agent(&temp).await;
        let (mut reader_a, mut writer_a) = connect_and_register(&sock_path, "client-a").await;
        load_session(&mut writer_a, "sess-union").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let spawned_live = r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"sess-union","update":{"sessionUpdate":"subagent_spawned","child_session_id":"child-union"}}}"#;
        response_tx.send(spawned_live.to_string()).unwrap();
        let _ = next_acp_payload_matching(&mut reader_a, "subagent_spawned").await;
        let (mut reader_b, mut writer_b, b_id) =
            connect_register_get_id(&sock_path, "client-b").await;
        load_session(&mut writer_b, "sess-union").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_b).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let spawned_replay = format!(
            r#"{{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{{"sessionId":"sess-union","_meta":{{"isReplay":true,"x.ai/leaderClientId":{}}},"update":{{"sessionUpdate":"subagent_spawned","child_session_id":"child-union"}}}}}}"#,
            b_id.0
        );
        response_tx.send(spawned_replay).unwrap();
        let _ = next_acp_payload_matching(&mut reader_b, "subagent_spawned").await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let child_live = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"child-union","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"UNION_DELTA"}}}}"#;
        response_tx.send(child_live.to_string()).unwrap();
        assert!(
            next_acp_payload_matching(&mut reader_a, "UNION_DELTA")
                .await
                .is_some(),
            "A's live route must survive B's replayed spawn (union, not replace)"
        );
        assert!(
            next_acp_payload_matching(&mut reader_b, "UNION_DELTA")
                .await
                .is_some(),
            "B is in the route too"
        );
        cancel.cancel();
    }
    /// A replayed finish that removes the LAST subscriber prunes the route,
    /// driver, and index edge — a later attacher must not be backfilled into
    /// a child whose finish was only ever observed via replay.
    #[tokio::test]
    async fn replayed_finished_last_subscriber_prunes_dead_child() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx, mut acp_rx) =
            setup_persistent_server_with_agent(&temp).await;
        let (mut reader_a, mut writer_a, a_id) =
            connect_register_get_id(&sock_path, "client-a").await;
        load_session(&mut writer_a, "sess-last").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let spawned_replay = format!(
            r#"{{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{{"sessionId":"sess-last","_meta":{{"isReplay":true,"x.ai/leaderClientId":{}}},"update":{{"sessionUpdate":"subagent_spawned","child_session_id":"child-last"}}}}}}"#,
            a_id.0
        );
        response_tx.send(spawned_replay).unwrap();
        let _ = next_acp_payload_matching(&mut reader_a, "subagent_spawned").await;
        let finished_replay = format!(
            r#"{{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{{"sessionId":"sess-last","_meta":{{"isReplay":true,"x.ai/leaderClientId":{}}},"update":{{"sessionUpdate":"subagent_finished","child_session_id":"child-last"}}}}}}"#,
            a_id.0
        );
        response_tx.send(finished_replay).unwrap();
        let _ = next_acp_payload_matching(&mut reader_a, "subagent_finished").await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let (mut reader_b, mut writer_b) = connect_and_register(&sock_path, "client-b").await;
        load_session(&mut writer_b, "sess-last").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_b).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let child_live = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"child-last","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"LAST_DELTA"}}}}"#;
        response_tx.send(child_live.to_string()).unwrap();
        assert!(
            next_acp_payload_matching(&mut reader_b, "LAST_DELTA")
                .await
                .is_none(),
            "the last-subscriber replayed finish must prune the edge"
        );
        assert!(
            next_acp_payload_matching(&mut reader_a, "LAST_DELTA")
                .await
                .is_none(),
            "A was unsubscribed by its own replayed finish"
        );
        cancel.cancel();
    }
    /// Isolates the REQUEST-side backfill call site: it subscribes the loader
    /// to live children the moment the `session/load` request passes through,
    /// so a child delta arriving MID-LOAD (post-request, pre-response) is
    /// delivered instead of dropped as subscriber-less. With only the
    /// response-side site the delta would be lost before the response lands.
    #[tokio::test]
    async fn mid_load_child_delta_reaches_loader_via_request_side_backfill() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx, mut acp_rx) =
            setup_persistent_server_with_agent(&temp).await;
        let (mut reader_a, mut writer_a) = connect_and_register(&sock_path, "client-a").await;
        load_session(&mut writer_a, "sess-midload").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let spawned_live = r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"sess-midload","update":{"sessionUpdate":"subagent_spawned","child_session_id":"child-midload"}}}"#;
        response_tx.send(spawned_live.to_string()).unwrap();
        let _ = next_acp_payload_matching(&mut reader_a, "subagent_spawned").await;
        drop(reader_a);
        drop(writer_a);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (mut reader_b, mut writer_b) = connect_and_register(&sock_path, "client-b").await;
        load_session(&mut writer_b, "sess-midload").await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let child_live = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"child-midload","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"MIDLOAD_DELTA"}}}}"#;
        response_tx.send(child_live.to_string()).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        complete_load(&mut acp_rx, &response_tx).await;
        assert!(
            next_acp_payload_matching(&mut reader_b, "MIDLOAD_DELTA")
                .await
                .is_some(),
            "a mid-load child delta must reach the loader (request-side backfill)"
        );
        cancel.cancel();
    }
    /// A pending interaction must SURVIVE a full client disconnect and be
    /// replayed on reconnect. A session with a pending interaction has a running
    /// turn (the tool awaits the answer), so the agent keeps it resident across
    /// the disconnect with the reverse-request still parked
    /// (`session_has_live_work`). The leader must therefore NOT drop its
    /// interaction cache on detach — otherwise the reconnecting client gets no
    /// modal while the agent is still waiting. Regression for the "modal vanishes
    /// on reconnect" bug.
    #[tokio::test]
    async fn pending_interaction_survives_disconnect_and_replays_on_reconnect() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx, mut acp_rx) =
            setup_persistent_server_with_agent(&temp).await;
        let (mut reader_a, mut writer_a) = connect_and_register(&sock_path, "client-a").await;
        load_session(&mut writer_a, "sess-int").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let req = r#"{"jsonrpc":"2.0","id":801,"method":"_x.ai/ask_user_question","params":{"method":"x.ai/ask_user_question","params":{"sessionId":"sess-int","toolCallId":"tc-reconnect","questions":[]}}}"#;
        response_tx.send(req.to_string()).unwrap();
        let _ = next_acp_payload_matching(&mut reader_a, "ask_user_question").await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(reader_a);
        drop(writer_a);
        tokio::time::sleep(Duration::from_millis(80)).await;
        let (mut reader_b, mut writer_b) = connect_and_register(&sock_path, "client-b").await;
        load_session(&mut writer_b, "sess-int").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let replayed = next_acp_payload_matching(&mut reader_b, "ask_user_question").await;
        assert!(
            replayed.is_some(),
            "a still-pending interaction must survive a full disconnect and replay on reconnect"
        );
        cancel.cancel();
    }
    /// An interaction raised while the session has NO subscriber (a session
    /// started from the dashboard whose turn hit `ask_user_question` before
    /// anyone entered it, or a reverse-request that races ahead of the
    /// `session/new`/`session/load` response that registers the subscriber) must
    /// still be cached, so the FIRST client to attach gets the modal replayed.
    /// Regression for the "entered the session, modal never appears, turn stuck
    /// Waiting" bug — the cache insert used to be gated on an existing subscriber.
    #[tokio::test]
    async fn interaction_raised_with_no_subscriber_is_cached_and_replayed_on_first_attach() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx, mut acp_rx) =
            setup_persistent_server_with_agent(&temp).await;
        let req = r#"{"jsonrpc":"2.0","id":901,"method":"_x.ai/ask_user_question","params":{"method":"x.ai/ask_user_question","params":{"sessionId":"sess-int","toolCallId":"tc-nosub","questions":[]}}}"#;
        response_tx.send(req.to_string()).unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        let (mut reader_a, mut writer_a) = connect_and_register(&sock_path, "client-a").await;
        load_session(&mut writer_a, "sess-int").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let replayed = next_acp_payload_matching(&mut reader_a, "ask_user_question").await;
        assert!(
            replayed.is_some(),
            "an interaction raised with no subscriber must be cached and replayed to the first client that attaches"
        );
        cancel.cancel();
    }
    /// Once an interaction resolves (first-answer-wins → `InteractionResolved`),
    /// the leader evicts it from the replay cache, so a client that attaches
    /// afterwards does NOT get a stale modal.
    #[tokio::test]
    async fn resolved_interaction_not_replayed_to_late_joiner() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx, mut acp_rx) =
            setup_persistent_server_with_agent(&temp).await;
        let (mut reader_a, mut writer_a) = connect_and_register(&sock_path, "client-a").await;
        load_session(&mut writer_a, "sess-int").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let _ = next_acp_payload(&mut reader_a).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let req = r#"{"jsonrpc":"2.0","id":701,"method":"_x.ai/ask_user_question","params":{"method":"x.ai/ask_user_question","params":{"sessionId":"sess-int","toolCallId":"tc-ev","questions":[]}}}"#;
        response_tx.send(req.to_string()).unwrap();
        let _ = next_acp_payload_matching(&mut reader_a, "ask_user_question").await;
        let resolved = r#"{"method":"_x.ai/session_notification","params":{"method":"x.ai/session_notification","params":{"sessionId":"sess-int","update":{"sessionUpdate":"interaction_resolved","tool_call_id":"tc-ev"}}}}"#;
        response_tx.send(resolved.to_string()).unwrap();
        let _ = next_acp_payload_matching(&mut reader_a, "interaction_resolved").await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let (mut reader_b, mut writer_b) = connect_and_register(&sock_path, "client-b").await;
        load_session(&mut writer_b, "sess-int").await;
        complete_load(&mut acp_rx, &response_tx).await;
        let replayed = next_acp_payload_matching(&mut reader_b, "ask_user_question").await;
        assert!(
            replayed.is_none(),
            "a resolved interaction must NOT be replayed to a late-joiner (evicted)"
        );
        cancel.cancel();
    }
    /// When the driver disconnects but another subscriber remains, the session
    /// is NOT evicted and the driver role transfers to the remaining client.
    #[tokio::test]
    async fn driver_disconnect_transfers_not_evicts() {
        let temp = TempDir::new().unwrap();
        let sock_path = temp.path().join("test.sock");
        let (acp_tx, mut acp_rx) = mpsc::unbounded_channel();
        let (response_tx, response_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let control_state = default_test_control_state(&sock_path);
        let sock_clone = sock_path.clone();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            let _ = run_leader_server(
                sock_clone,
                acp_tx,
                response_rx,
                cancel_clone,
                true,
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicBool::new(false)),
                AgentActivity::default(),
                watch::channel(true).1,
                watch::channel(false).0,
                watch::channel(super::super::protocol::ShutdownReason::Manual).0,
                None,
                control_state,
            )
            .await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (reader_a, mut writer_a) = connect_and_register(&sock_path, "client-a").await;
        load_session(&mut writer_a, "sess-xfer").await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let (mut reader_b, mut writer_b) = connect_and_register(&sock_path, "client-b").await;
        load_session(&mut writer_b, "sess-xfer").await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        while acp_rx.try_recv().is_ok() {}
        write_message(&mut writer_a, &ClientMessage::Disconnect)
            .await
            .unwrap();
        drop(reader_a);
        drop(writer_a);
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            acp_rx.try_recv().is_err(),
            "session must NOT be evicted while another subscriber remains"
        );
        let req = r#"{"jsonrpc":"2.0","id":7,"method":"fs/read_text_file","params":{"sessionId":"sess-xfer","path":"/tmp/x"}}"#;
        response_tx.send(req.to_string()).unwrap();
        let req_b = next_acp_payload(&mut reader_b).await;
        assert!(
            req_b
                .as_deref()
                .is_some_and(|p| p.contains("read_text_file")),
            "after driver disconnect, B should become driver and receive the reverse-request, got {req_b:?}"
        );
        cancel.cancel();
    }
    /// `x.ai/sessions/changed` is a machine-wide roster notification with no
    /// sessionId; it must broadcast to every registered client (not just the
    /// last-active one) so all open dashboards stay in sync.
    #[tokio::test]
    async fn roster_changed_broadcasts_to_all_clients() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx) = setup_persistent_server(&temp).await;
        let (mut reader_a, _writer_a) = connect_and_register(&sock_path, "client-a").await;
        let (mut reader_b, _writer_b) = connect_and_register(&sock_path, "client-b").await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let changed = r#"{"jsonrpc":"2.0","method":"x.ai/sessions/changed","params":{"upserted":[{"sessionId":"sess-roster","cwd":"/repo","isWorktree":false,"yolo":false,"activity":"working","resident":true,"lastChangeUnixMs":1,"origin":{"kind":"local"}}],"removed":[]}}"#;
        response_tx.send(changed.to_string()).unwrap();
        let got_a = next_acp_payload(&mut reader_a).await;
        let got_b = next_acp_payload(&mut reader_b).await;
        assert!(
            got_a.as_deref().is_some_and(|p| p.contains("sess-roster")),
            "client A must receive the roster broadcast, got {got_a:?}"
        );
        assert!(
            got_b.as_deref().is_some_and(|p| p.contains("sess-roster")),
            "client B must receive the roster broadcast, got {got_b:?}"
        );
        cancel.cancel();
    }
    /// `x.ai/models/update` is a machine-wide catalog notification with no
    /// sessionId; it must broadcast to every registered client so every model
    /// picker refreshes after a config.toml / models_cache.json hot-reload —
    /// not just the last-active client. Uses the production wire form: agent
    /// ext notifications arrive `_`-prefixed (`_x.ai/models/update`).
    #[tokio::test]
    async fn models_update_broadcasts_to_all_clients() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx) = setup_persistent_server(&temp).await;
        let (mut reader_a, _writer_a) = connect_and_register(&sock_path, "client-a").await;
        let (mut reader_b, _writer_b) = connect_and_register(&sock_path, "client-b").await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let update = r#"{"jsonrpc":"2.0","method":"_x.ai/models/update","params":{"currentModelId":"grok-new","availableModels":[{"modelId":"grok-new","name":"Grok New"}]}}"#;
        response_tx.send(update.to_string()).unwrap();
        let got_a = next_acp_payload(&mut reader_a).await;
        let got_b = next_acp_payload(&mut reader_b).await;
        assert!(
            got_a.as_deref().is_some_and(|p| p.contains("grok-new")),
            "client A must receive the models broadcast, got {got_a:?}"
        );
        assert!(
            got_b.as_deref().is_some_and(|p| p.contains("grok-new")),
            "client B must receive the models broadcast, got {got_b:?}"
        );
        cancel.cancel();
    }
    /// `x.ai/mcp/servers_updated` is a machine-wide MCP-catalog notification
    /// with no sessionId (session-agnostic by design); it must broadcast to
    /// every registered client so managed connectors don't vanish from clients
    /// that weren't last-active when the post-initialize background fetch
    /// resolved. Uses the production wire form (`_`-prefixed ext notification
    /// with the real method nested in params).
    #[tokio::test]
    async fn mcp_servers_updated_broadcasts_to_all_clients() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx) = setup_persistent_server(&temp).await;
        let (mut reader_a, _writer_a) = connect_and_register(&sock_path, "client-a").await;
        let (mut reader_b, _writer_b) = connect_and_register(&sock_path, "client-b").await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let update = r#"{"jsonrpc":"2.0","method":"_x.ai/mcp/servers_updated","params":{"method":"x.ai/mcp/servers_updated","params":{"mcpServers":[{"name":"grok_com_slack","source":"managed"}]}}}"#;
        response_tx.send(update.to_string()).unwrap();
        let got_a = next_acp_payload(&mut reader_a).await;
        let got_b = next_acp_payload(&mut reader_b).await;
        assert!(
            got_a
                .as_deref()
                .is_some_and(|p| p.contains("grok_com_slack")),
            "client A must receive the MCP catalog broadcast, got {got_a:?}"
        );
        assert!(
            got_b
                .as_deref()
                .is_some_and(|p| p.contains("grok_com_slack")),
            "client B must receive the MCP catalog broadcast, got {got_b:?}"
        );
        cancel.cancel();
    }
    /// The broadcast classifier must accept both wire forms (`_`-prefixed
    /// production ext notifications and direct methods) for the machine-wide
    /// set, and reject sessionful / unrelated methods.
    #[test]
    fn machine_wide_broadcast_classifier_matches_both_wire_forms() {
        assert!(is_machine_wide_broadcast_notification(&pv(
            r#"{"jsonrpc":"2.0","method":"x.ai/sessions/changed","params":{}}"#
        )));
        assert!(is_machine_wide_broadcast_notification(&pv(
            r#"{"jsonrpc":"2.0","method":"x.ai/models/update","params":{}}"#
        )));
        assert!(is_machine_wide_broadcast_notification(&pv(
            r#"{"jsonrpc":"2.0","method":"x.ai/mcp/servers_updated","params":{}}"#
        )));
        assert!(is_machine_wide_broadcast_notification(&pv(
            r#"{"jsonrpc":"2.0","method":"x.ai/announcements/update","params":{}}"#
        )));
        assert!(is_machine_wide_broadcast_notification(&pv(
            r#"{"jsonrpc":"2.0","method":"_x.ai/sessions/changed","params":{}}"#
        )));
        assert!(is_machine_wide_broadcast_notification(&pv(
            r#"{"jsonrpc":"2.0","method":"_x.ai/models/update","params":{}}"#
        )));
        assert!(is_machine_wide_broadcast_notification(&pv(
            r#"{"jsonrpc":"2.0","method":"_x.ai/mcp/servers_updated","params":{"method":"x.ai/mcp/servers_updated","params":{"mcpServers":[]}}}"#
        )));
        assert!(is_machine_wide_broadcast_notification(&pv(
            r#"{"jsonrpc":"2.0","method":"_x.ai/announcements/update","params":{"method":"x.ai/announcements/update","params":{"gen":2,"announcements":[]}}}"#
        )));
        assert!(!is_machine_wide_broadcast_notification(&pv(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s"}}"#
        )));
        assert!(!is_machine_wide_broadcast_notification(&pv(
            r#"{"jsonrpc":"2.0","method":"x.ai/settings/update","params":{}}"#
        )));
    }
    /// Verify that the leader injects `codeNavEnabled: true` into session/new
    /// when the client registered with `code_nav_enabled: true`.
    #[test]
    fn inject_capabilities_sets_code_nav_enabled_true() {
        let caps = ClientCapabilities {
            yolo_mode: false,
            default_model: None,
            client_version: None,
            code_nav_enabled: true,
            ..Default::default()
        };
        let payload = r#"{"jsonrpc":"2.0","method":"session/new","id":1,"params":{"cwd":"/repo","_meta":{}}}"#;
        let mut json = pv(payload);
        inject_capabilities_into_session_new(&mut json, &caps, "grok-web", ClientId(1));
        assert_eq!(
            json["params"]["_meta"]["codeNavEnabled"],
            serde_json::json!(true),
            "leader must inject codeNavEnabled=true for code-nav-capable client"
        );
    }
    /// Verify that the leader injects `codeNavEnabled: false` when the client
    /// did NOT register with `code_nav_enabled` — preventing a prior eligible
    /// client's shared state from bleeding into this client's sessions.
    #[test]
    fn inject_capabilities_sets_code_nav_enabled_false() {
        let caps = ClientCapabilities {
            yolo_mode: false,
            default_model: None,
            client_version: None,
            code_nav_enabled: false,
            ..Default::default()
        };
        let payload = r#"{"jsonrpc":"2.0","method":"session/new","id":1,"params":{"cwd":"/repo","_meta":{"clientIdentifier":"grok-tui"}}}"#;
        let mut json = pv(payload);
        inject_capabilities_into_session_new(&mut json, &caps, "grok-tui", ClientId(1));
        assert_eq!(
            json["params"]["_meta"]["codeNavEnabled"],
            serde_json::json!(false),
            "leader must inject codeNavEnabled=false for client without code-nav capability"
        );
    }
    /// Verify that `codeNavEnabled` is also injected into `session/load` so
    /// reconnect sessions inherit the correct per-client capability.
    #[test]
    fn inject_capabilities_injects_code_nav_into_session_load() {
        let caps = ClientCapabilities {
            yolo_mode: false,
            default_model: None,
            client_version: None,
            code_nav_enabled: true,
            ..Default::default()
        };
        let payload = r#"{"jsonrpc":"2.0","method":"session/load","id":2,"params":{"sessionId":"abc","cwd":"/repo","_meta":{}}}"#;
        let mut json = pv(payload);
        inject_capabilities_into_session_new(&mut json, &caps, "grok-web", ClientId(1));
        assert_eq!(
            json["params"]["_meta"]["codeNavEnabled"],
            serde_json::json!(true),
            "leader must inject codeNavEnabled into session/load for reconnect isolation"
        );
    }
    /// Verify leader-mode client isolation: two clients with different code-nav
    /// capabilities get independent `codeNavEnabled` values injected into their
    /// session/new requests.
    #[test]
    fn inject_capabilities_two_clients_stay_isolated() {
        let web_caps = ClientCapabilities {
            code_nav_enabled: true,
            ..Default::default()
        };
        let tui_caps = ClientCapabilities {
            code_nav_enabled: false,
            ..Default::default()
        };
        let session_new = r#"{"jsonrpc":"2.0","method":"session/new","id":1,"params":{"cwd":"/repo","_meta":{}}}"#;
        let mut web_json = pv(session_new);
        inject_capabilities_into_session_new(&mut web_json, &web_caps, "grok-web", ClientId(1));
        let mut tui_json = pv(session_new);
        inject_capabilities_into_session_new(&mut tui_json, &tui_caps, "grok-tui", ClientId(2));
        assert_eq!(
            web_json["params"]["_meta"]["codeNavEnabled"],
            serde_json::json!(true)
        );
        assert_eq!(
            tui_json["params"]["_meta"]["codeNavEnabled"],
            serde_json::json!(false)
        );
    }
    #[test]
    fn inject_capabilities_terminal_and_fs_per_client() {
        let web_caps = ClientCapabilities {
            terminal: true,
            fs_read: true,
            fs_write: true,
            ..Default::default()
        };
        let tui_caps = ClientCapabilities {
            terminal: false,
            fs_read: false,
            fs_write: false,
            ..Default::default()
        };
        let session_new = r#"{"jsonrpc":"2.0","method":"session/new","id":1,"params":{"cwd":"/repo","_meta":{}}}"#;
        let mut web_json = pv(session_new);
        inject_capabilities_into_session_new(&mut web_json, &web_caps, "grok-web", ClientId(1));
        let mut tui_json = pv(session_new);
        inject_capabilities_into_session_new(&mut tui_json, &tui_caps, "grok-tui", ClientId(2));
        assert_eq!(
            web_json["params"]["_meta"]["clientTerminal"],
            serde_json::json!(true)
        );
        assert_eq!(
            web_json["params"]["_meta"]["clientFsRead"],
            serde_json::json!(true)
        );
        assert_eq!(
            web_json["params"]["_meta"]["clientFsWrite"],
            serde_json::json!(true)
        );
        assert_eq!(
            tui_json["params"]["_meta"]["clientTerminal"],
            serde_json::json!(false)
        );
        assert_eq!(
            tui_json["params"]["_meta"]["clientFsRead"],
            serde_json::json!(false)
        );
        assert_eq!(
            tui_json["params"]["_meta"]["clientFsWrite"],
            serde_json::json!(false)
        );
    }
    #[test]
    fn inject_capabilities_terminal_into_session_load() {
        let caps = ClientCapabilities {
            terminal: true,
            fs_read: false,
            fs_write: false,
            ..Default::default()
        };
        let session_load = r#"{"jsonrpc":"2.0","method":"session/load","id":2,"params":{"sessionId":"sess-1","_meta":{}}}"#;
        let mut json = pv(session_load);
        inject_capabilities_into_session_new(&mut json, &caps, "grok-web", ClientId(1));
        assert_eq!(
            json["params"]["_meta"]["clientTerminal"],
            serde_json::json!(true)
        );
        assert_eq!(
            json["params"]["_meta"]["clientFsRead"],
            serde_json::json!(false)
        );
        assert_eq!(
            json["params"]["_meta"]["clientFsWrite"],
            serde_json::json!(false)
        );
    }
    #[tokio::test]
    async fn subagent_child_session_routed_after_spawned() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx) = setup_persistent_server(&temp).await;
        let (mut reader, mut writer) = connect_and_register(&sock_path, "test").await;
        write_message(
                &mut writer,
                &ClientMessage::Acp {
                    payload: r#"{"jsonrpc":"2.0","method":"session/prompt","id":1,"params":{"sessionId":"sess-parent","prompt":[]}}"#
                        .into(),
                },
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        response_tx
            .send(
                r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"sess-parent","update":{"sessionUpdate":"subagent_spawned","child_session_id":"child-123"}}}"#
                    .into(),
            )
            .unwrap();
        let _: ServerMessage =
            tokio::time::timeout(Duration::from_millis(200), read_message(&mut reader))
                .await
                .unwrap()
                .unwrap();
        response_tx
            .send(
                r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"child-123","update":{"sessionUpdate":"message_delta","content":"hello"}}}"#
                    .into(),
            )
            .unwrap();
        let msg: ServerMessage =
            tokio::time::timeout(Duration::from_millis(200), read_message(&mut reader))
                .await
                .expect("child session notification should reach parent owner")
                .unwrap();
        match msg {
            ServerMessage::Acp { payload } => {
                let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
                assert_eq!(json["params"]["sessionId"], "child-123");
            }
            other => panic!("Expected Acp, got {:?}", other),
        }
        cancel.cancel();
    }
    #[tokio::test]
    async fn subagent_child_session_cleaned_up_on_finished() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx) = setup_persistent_server(&temp).await;
        let (mut reader, mut writer) = connect_and_register(&sock_path, "test").await;
        write_message(
                &mut writer,
                &ClientMessage::Acp {
                    payload: r#"{"jsonrpc":"2.0","method":"session/prompt","id":1,"params":{"sessionId":"sess-parent","prompt":[]}}"#
                        .into(),
                },
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        response_tx
            .send(
                r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"sess-parent","update":{"sessionUpdate":"subagent_spawned","child_session_id":"child-456"}}}"#
                    .into(),
            )
            .unwrap();
        let _: ServerMessage =
            tokio::time::timeout(Duration::from_millis(200), read_message(&mut reader))
                .await
                .unwrap()
                .unwrap();
        response_tx
            .send(
                r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"sess-parent","update":{"sessionUpdate":"subagent_finished","child_session_id":"child-456"}}}"#
                    .into(),
            )
            .unwrap();
        let _: ServerMessage =
            tokio::time::timeout(Duration::from_millis(200), read_message(&mut reader))
                .await
                .unwrap()
                .unwrap();
        response_tx
            .send(
                r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"child-456","update":{"sessionUpdate":"message_delta"}}}"#
                    .into(),
            )
            .unwrap();
        let timeout_result: Result<Result<ServerMessage, _>, _> =
            tokio::time::timeout(Duration::from_millis(100), read_message(&mut reader)).await;
        assert!(
            timeout_result.is_err(),
            "Notification for finished child session should not be routed"
        );
        cancel.cancel();
    }
    #[tokio::test]
    async fn subagent_child_session_not_leaked_to_other_client() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx) = setup_persistent_server(&temp).await;
        let (mut reader_a, mut writer_a) = connect_and_register(&sock_path, "test-a").await;
        write_message(
                &mut writer_a,
                &ClientMessage::Acp {
                    payload: r#"{"jsonrpc":"2.0","method":"session/prompt","id":1,"params":{"sessionId":"sess-parent","prompt":[]}}"#
                        .into(),
                },
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        response_tx
            .send(
                r#"{"jsonrpc":"2.0","method":"_x.ai/session_notification","params":{"method":"x.ai/session_notification","params":{"sessionId":"sess-parent","update":{"sessionUpdate":"subagent_spawned","child_session_id":"child-789"}}}}"#
                    .into(),
            )
            .unwrap();
        let _: ServerMessage =
            tokio::time::timeout(Duration::from_millis(200), read_message(&mut reader_a))
                .await
                .unwrap()
                .unwrap();
        let (mut reader_b, mut writer_b) = connect_and_register(&sock_path, "test-b").await;
        write_message(
            &mut writer_b,
            &ClientMessage::Acp {
                payload: r#"{"jsonrpc":"2.0","method":"initialize","id":2,"params":{}}"#.into(),
            },
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        response_tx
            .send(
                r#"{"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":"child-789","update":{"sessionUpdate":"message_delta"}}}"#
                    .into(),
            )
            .unwrap();
        let msg: ServerMessage =
            tokio::time::timeout(Duration::from_millis(200), read_message(&mut reader_a))
                .await
                .expect("Client A should receive child session notification")
                .unwrap();
        assert!(matches!(msg, ServerMessage::Acp { .. }));
        let timeout_result: Result<Result<ServerMessage, _>, _> =
            tokio::time::timeout(Duration::from_millis(100), read_message(&mut reader_b)).await;
        assert!(
            timeout_result.is_err(),
            "Client B should NOT receive child session notification"
        );
        cancel.cancel();
    }
    #[tokio::test]
    async fn leader_client_id_unicasts_to_target_only() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx) = setup_persistent_server(&temp).await;
        async fn register_capture(
            sock_path: &std::path::Path,
            client_type: &str,
        ) -> (
            tokio::io::ReadHalf<LeaderStream>,
            tokio::io::WriteHalf<LeaderStream>,
            u64,
        ) {
            let stream = LeaderStream::connect(sock_path).await.unwrap();
            let (mut reader, mut writer) = tokio::io::split(stream);
            write_message(
                &mut writer,
                &ClientMessage::Register {
                    client_type: client_type.into(),
                    mode: ClientMode::Stdio,
                    capabilities: ClientCapabilities::default(),
                },
            )
            .await
            .unwrap();
            let msg: ServerMessage = read_message(&mut reader).await.unwrap();
            let client_id = match msg {
                ServerMessage::Registered { client_id, .. } => client_id,
                other => panic!("Expected Registered, got {:?}", other),
            };
            (reader, writer, client_id)
        }
        let (mut reader_a, _writer_a, id_a) = register_capture(&sock_path, "test-a").await;
        let (mut reader_b, _writer_b, _id_b) = register_capture(&sock_path, "test-b").await;
        response_tx
            .send(
                format!(
                    r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"sess-1","update":{{"sessionUpdate":"agent_message_chunk"}},"_meta":{{"x.ai/leaderClientId":{}}}}}}}"#,
                    id_a
                ),
            )
            .unwrap();
        let msg = tokio::time::timeout(Duration::from_millis(200), read_message(&mut reader_a))
            .await
            .expect("Client A should receive the tagged replay notification")
            .unwrap();
        assert!(matches!(msg, ServerMessage::Acp { .. }));
        let timeout_result: Result<Result<ServerMessage, _>, _> =
            tokio::time::timeout(Duration::from_millis(100), read_message(&mut reader_b)).await;
        assert!(
            timeout_result.is_err(),
            "Client B must not receive a notification tagged for client A"
        );
        cancel.cancel();
    }
    #[tokio::test]
    async fn leader_client_id_dropped_when_target_disconnected() {
        let temp = TempDir::new().unwrap();
        let (sock_path, cancel, response_tx) = setup_persistent_server(&temp).await;
        let stream_a = LeaderStream::connect(&sock_path).await.unwrap();
        let (mut reader_a, mut writer_a) = tokio::io::split(stream_a);
        write_message(
            &mut writer_a,
            &ClientMessage::Register {
                client_type: "test-a".into(),
                mode: ClientMode::Stdio,
                capabilities: ClientCapabilities::default(),
            },
        )
        .await
        .unwrap();
        let id_a = match read_message(&mut reader_a).await.unwrap() {
            ServerMessage::Registered { client_id, .. } => client_id,
            other => panic!("Expected Registered, got {:?}", other),
        };
        let (mut reader_b, mut writer_b) = connect_and_register(&sock_path, "test-b").await;
        write_message(
                &mut writer_b,
                &ClientMessage::Acp {
                    payload: r#"{"jsonrpc":"2.0","method":"session/prompt","id":1,"params":{"sessionId":"sess-1","prompt":[]}}"#
                        .into(),
                },
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(reader_a);
        drop(writer_a);
        tokio::time::sleep(Duration::from_millis(100)).await;
        response_tx
            .send(
                format!(
                    r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"sess-1","update":{{"sessionUpdate":"agent_message_chunk"}},"_meta":{{"isReplay":true,"x.ai/leaderClientId":{}}}}}}}"#,
                    id_a
                ),
            )
            .unwrap();
        response_tx
            .send(
                format!(
                    r#"{{"jsonrpc":"2.0","method":"_x.ai/session/update","params":{{"params":{{"sessionId":"sess-1","update":{{"sessionUpdate":"hook_annotation","message":"m"}},"_meta":{{"isReplay":true,"x.ai/leaderClientId":{}}}}}}}}}"#,
                    id_a
                ),
            )
            .unwrap();
        let timeout_result: Result<Result<ServerMessage, _>, _> =
            tokio::time::timeout(Duration::from_millis(150), read_message(&mut reader_b)).await;
        assert!(
            timeout_result.is_err(),
            "A targeted replay line for a disconnected loader must be dropped, not broadcast"
        );
        response_tx
            .send(
                r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk"}}}"#
                    .into(),
            )
            .unwrap();
        let msg = tokio::time::timeout(Duration::from_millis(200), read_message(&mut reader_b))
            .await
            .expect("Subscriber B should still receive untagged live notifications")
            .unwrap();
        assert!(matches!(msg, ServerMessage::Acp { .. }));
        cancel.cancel();
    }
}
