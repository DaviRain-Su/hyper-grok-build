//! MCP integration for the workspace server.
//!
//! Bridges [`McpClient`] to the server's [`McpTransport`] trait.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{FuturesUnordered, StreamExt};
use serde_json::Value;
use xai_computer_hub_mcp_adapter::{
    McpBridge, McpBridgeConfig, McpBridgeHandle, McpCallResult, McpContent, McpServerInfo,
    McpToolDefinition, McpTransport,
};
use xai_computer_hub_sdk::ToolServerHandler;
use xai_grok_mcp::rmcp;
use xai_grok_mcp::servers::{
    MCP_TOOL_NAME_DELIMITER, McpClient, McpClientTimeoutOverrides, McpSpawnCtx, OauthInteractivity,
    parse_mcp_qualified_name,
};
use xai_tool_protocol::{SessionId, ToolId};
use xai_tool_runtime::{ToolCallContext, ToolStream, TypedToolOutput};
use xai_tool_types::ToolDescription;

use crate::error::{WorkspaceError, WorkspaceResult};
use crate::session::{SessionMcpServer, WorkspaceMcpBinding, WorkspaceSession};

/// The slice of the hub's per-session tool surface that MCP reconfiguration
/// drives: what a session currently advertises, plus dynamic registration in
/// both directions.
///
/// Implemented by the production
/// [`ToolServer`](xai_computer_hub_sdk::ToolServer). The seam exists so the
/// reload tests can substitute an in-memory hub — a real `ToolServer` cannot
/// be built without a live hub connection.
pub(crate) trait HubToolRegistry: Send + Sync {
    /// `life` is the session's MCP life epoch the registration belongs to:
    /// the hub ledger is life-tagged, so a stale unregister (a teardown of
    /// a life a revive has superseded) can never remove a newer life's
    /// registration, and an unregister never touches a non-dynamic handler
    /// (e.g. a resolver-installed native sharing the id).
    fn register_tool_dynamic(
        &self,
        handler: Arc<dyn ToolServerHandler>,
        sessions: Vec<SessionId>,
        life: u64,
    ) -> impl Future<Output = Result<(), xai_computer_hub_sdk::ClientError>> + Send;

    fn unregister_tool_dynamic(
        &self,
        tool_id: &ToolId,
        session_id: &SessionId,
        life: u64,
    ) -> impl Future<Output = Result<bool, xai_computer_hub_sdk::ClientError>> + Send;
}

impl HubToolRegistry for xai_computer_hub_sdk::ToolServer {
    async fn register_tool_dynamic(
        &self,
        handler: Arc<dyn ToolServerHandler>,
        sessions: Vec<SessionId>,
        life: u64,
    ) -> Result<(), xai_computer_hub_sdk::ClientError> {
        Self::register_tool_dynamic(self, handler, sessions, life).await
    }

    async fn unregister_tool_dynamic(
        &self,
        tool_id: &ToolId,
        session_id: &SessionId,
        life: u64,
    ) -> Result<bool, xai_computer_hub_sdk::ClientError> {
        Self::unregister_tool_dynamic(self, tool_id, session_id, life).await
    }
}

/// Adapts [`McpClient`] to the [`McpTransport`] trait for [`McpBridge`].
pub(crate) struct McpClientTransportAdapter {
    client: Arc<McpClient>,
}

impl McpClientTransportAdapter {
    pub fn new(client: Arc<McpClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl McpTransport for McpClientTransportAdapter {
    async fn initialize(&self) -> Result<McpServerInfo, xai_computer_hub_mcp_adapter::McpError> {
        let service = self
            .client
            .ensure_initialized()
            .await
            .map_err(|e| xai_computer_hub_mcp_adapter::McpError::Transport(e.to_string()))?;
        let info = service.peer_info().ok_or_else(|| {
            xai_computer_hub_mcp_adapter::McpError::Transport("no peer info after init".into())
        })?;
        // rmcp 3.x makes the server implementation identity optional on peer info.
        let server_info = info.server_info.as_ref();
        Ok(McpServerInfo {
            name: server_info.map(|si| si.name.clone()).unwrap_or_default(),
            version: server_info.map(|si| si.version.clone()).unwrap_or_default(),
            capabilities: serde_json::to_value(&info.capabilities).unwrap_or_default(),
        })
    }

    async fn list_tools(
        &self,
    ) -> Result<Vec<McpToolDefinition>, xai_computer_hub_mcp_adapter::McpError> {
        let service = self
            .client
            .ensure_initialized()
            .await
            .map_err(|e| xai_computer_hub_mcp_adapter::McpError::Transport(e.to_string()))?;

        let mut all_tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let result = service
                .list_tools(Some(
                    rmcp::model::PaginatedRequestParams::default().with_cursor(cursor.clone()),
                ))
                .await
                .map_err(|e| xai_computer_hub_mcp_adapter::McpError::Transport(e.to_string()))?;

            all_tools.extend(result.tools.into_iter().map(|t| McpToolDefinition {
                name: t.name.to_string(),
                description: t.description.map(|d| d.to_string()),
                input_schema: serde_json::to_value(&t.input_schema).ok(),
            }));

            match result.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        Ok(all_tools)
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<McpCallResult, xai_computer_hub_mcp_adapter::McpError> {
        let service = self
            .client
            .ensure_initialized()
            .await
            .map_err(|e| xai_computer_hub_mcp_adapter::McpError::Transport(e.to_string()))?;
        // MCP spec requires arguments to be an object; coerce if needed.
        let args_object = match arguments {
            Value::Object(map) => Some(map),
            Value::Null => None,
            other => {
                let mut wrapper = serde_json::Map::new();
                wrapper.insert("value".to_string(), other);
                Some(wrapper)
            }
        };
        let result = service
            .call_tool({
                let mut params = rmcp::model::CallToolRequestParams::new(name.to_string());
                params.arguments = args_object;
                params
            })
            .await
            .map_err(|e| xai_computer_hub_mcp_adapter::McpError::Transport(e.to_string()))?;

        Ok(McpCallResult {
            content: result
                .content
                .into_iter()
                .map(|c| match c {
                    rmcp::model::ContentBlock::Text(t) => McpContent::Text { text: t.text },
                    rmcp::model::ContentBlock::Image(img) => McpContent::Image {
                        mime_type: img.mime_type,
                        data: img.data,
                    },
                    _ => McpContent::Text {
                        text: "[unsupported content type]".to_string(),
                    },
                })
                .collect(),
            is_error: result.is_error.unwrap_or(false),
        })
    }

    async fn close(&self) -> Result<(), xai_computer_hub_mcp_adapter::McpError> {
        // No-op: cleanup happens when McpClient is dropped.
        Ok(())
    }
}

/// Preserves the legacy `server__tool` names used by `workspace.configure_mcp`.
pub(crate) struct QualifiedMcpToolHandler {
    tool_id: ToolId,
    inner: Arc<dyn ToolServerHandler>,
}

impl QualifiedMcpToolHandler {
    pub fn from_namespaced(inner: Arc<dyn ToolServerHandler>) -> Option<Self> {
        let description = inner.description();
        let namespace = description.namespace?;
        let qualified_name = format!("{namespace}{MCP_TOOL_NAME_DELIMITER}{}", inner.tool_id());
        let Some((tool_id, _, _)) = parse_mcp_qualified_name(&qualified_name) else {
            tracing::warn!(qualified_name, "skipping invalid qualified MCP tool name");
            return None;
        };
        Some(Self { tool_id, inner })
    }
}

#[async_trait]
impl ToolServerHandler for QualifiedMcpToolHandler {
    fn tool_id(&self) -> ToolId {
        self.tool_id.clone()
    }

    fn description(&self) -> ToolDescription {
        let inner = self.inner.description();
        ToolDescription::new(self.tool_id.as_str().to_owned(), inner.description)
    }

    fn input_schema(&self) -> Option<Value> {
        self.inner.input_schema()
    }

    async fn handle_call(&self, ctx: ToolCallContext, args: Value) -> ToolStream<TypedToolOutput> {
        self.inner.handle_call(ctx, args).await
    }
}

/// Result of a `workspace.configure_mcp` RPC call.
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpStartResult {
    /// Server names that started successfully.
    pub started: Vec<String>,
    pub failed: Vec<McpStartFailure>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct McpStartFailure {
    pub name: String,
    /// Human-readable error description.
    pub error: String,
}

/// One MCP server that started and finished tool discovery. The bridge holds
/// the tools it discovered.
pub(crate) struct StartedMcpServer {
    pub(crate) name: String,
    pub(crate) bridge: McpBridgeHandle,
}

/// Outcome of [`connect_servers`].
pub(crate) struct StartedMcp {
    pub(crate) servers: Vec<StartedMcpServer>,
    pub(crate) failed: Vec<McpStartFailure>,
}

type BridgeOutcome = Result<(String, Arc<McpClient>, McpBridgeHandle), McpStartFailure>;

/// Record one server's start outcome in the session's [`McpState`] and
/// translate it for the caller. Returns `None` when the LIFE the drive
/// started under is no longer current — the client is dropped, which
/// closes its transport and kills its process.
async fn record_bridge_outcome(
    outcome: BridgeOutcome,
    session: &WorkspaceSession,
    remaining_names: &mut HashSet<String>,
    life: u64,
) -> Option<Result<StartedMcpServer, McpStartFailure>> {
    match outcome {
        Ok((server_name, client, bridge)) => {
            remaining_names.remove(&server_name);
            // Commit gate, life-coherent by construction: the binding lock
            // is HELD across the `owned_clients` write (lock order binding →
            // mcp_state, the same order teardown's in-critical-section sweep
            // uses), and the gate compares the LIFE — not merely
            // not-`Closed`. A teardown-then-revive during the server's start
            // leaves the binding `Active` again, but for a NEW life; a
            // state-only gate would commit this stale client into it (and
            // publish the outcome to the drive's consumer). Epoch equality
            // subsumes the `Closed` check: teardown bumps before flipping.
            let stale = {
                let _binding = session.mcp_binding.lock().await;
                let stale = session.mcp_epoch.load(std::sync::atomic::Ordering::SeqCst) != life;
                let mut state = session.mcp_state.lock().await;
                if stale {
                    state.owned_clients.remove(&server_name);
                } else {
                    state.owned_clients.insert(server_name.clone(), client);
                    state.mark_server_ready(&server_name);
                }
                stale
            };
            if stale {
                tracing::info!(
                    server = %server_name,
                    "dropping MCP client that finished connecting after its life ended"
                );
                return None;
            }
            Some(Ok(StartedMcpServer {
                name: server_name,
                bridge,
            }))
        }
        Err(failure) => {
            remaining_names.remove(&failure.name);
            {
                // Same nesting and life gate as the Ok arm, so a failure's
                // bookkeeping cannot race a teardown's sweep or land in a
                // revived life's state.
                let _binding = session.mcp_binding.lock().await;
                if session.mcp_epoch.load(std::sync::atomic::Ordering::SeqCst) == life {
                    let mut state = session.mcp_state.lock().await;
                    state.owned_clients.remove(&failure.name);
                    state.record_init_failure(&failure.name, false, Some(failure.error.clone()));
                    state.mark_server_ready(&failure.name);
                }
            }
            tracing::warn!(
                server = %failure.name,
                error = %failure.error,
                "McpBridge::connect failed"
            );
            Some(Err(failure))
        }
    }
}

/// Decide what each of a session's servers may advertise, record it on the
/// server, and return the flat list.
///
/// An id is dropped when a `native` tool already holds it, or when two
/// servers both offer it — an ambiguous id is refused outright rather than
/// settled by whichever server started first. Servers are visited in name
/// order so the advertised list is stable across binds.
///
/// Hard bound on the MCP tools one session advertises, across all of its
/// servers. Tool lists come from external servers and every advertised tool
/// is model-visible, so the fan-in carries an explicit cap; tools past it
/// are dropped deterministically (servers in name order, tools in server
/// order) and stay unowned, so a later removal cannot unregister a tool
/// that was never advertised.
pub(crate) const MAX_ADVERTISED_MCP_TOOLS: usize = 256;

/// The configured path's only writer of `tool_ids`, which is what keeps a
/// removal unregistering exactly what its server contributed. (The legacy
/// qualified path has its own, mutually exclusive writer:
/// [`install_and_advertise_qualified`].)
pub(crate) async fn claim_tools(
    session: &WorkspaceSession,
    native: &HashSet<ToolId>,
) -> (Vec<Arc<dyn ToolServerHandler>>, u64) {
    let mut binding = session.mcp_binding.lock().await;
    // The life these claims belong to, observed under the same lock that
    // recorded them; `settle_registrations` re-checks it so registrations
    // made for this life are never committed to (or left registered under)
    // a newer one.
    let life = session.mcp_epoch.load(std::sync::atomic::Ordering::SeqCst);
    let Some(active) = binding.active_mut() else {
        return (Vec::new(), life);
    };

    let mut offers: HashMap<ToolId, usize> = HashMap::new();
    for server in active.servers.values() {
        for handler in server.bridge.bridge.handlers() {
            *offers.entry(handler.tool_id()).or_default() += 1;
        }
    }

    let mut names: Vec<String> = active.servers.keys().cloned().collect();
    names.sort();
    let mut advertised = Vec::new();
    let mut rejected = 0usize;
    let mut over_cap = 0usize;
    for name in names {
        let Some(server) = active.servers.get_mut(&name) else {
            continue;
        };
        server.tool_ids.clear();
        for handler in server.bridge.bridge.handlers() {
            let tool_id = handler.tool_id();
            if native.contains(&tool_id) || offers.get(&tool_id) != Some(&1usize) {
                rejected += 1;
                continue;
            }
            if advertised.len() >= MAX_ADVERTISED_MCP_TOOLS {
                over_cap += 1;
                continue;
            }
            server.tool_ids.push(tool_id);
            advertised.push(Arc::clone(handler) as Arc<dyn ToolServerHandler>);
        }
    }
    if rejected > 0 {
        tracing::warn!(
            rejected,
            "skipping MCP tools whose IDs collide with native or sibling MCP tools"
        );
    }
    if over_cap > 0 {
        tracing::warn!(
            over_cap,
            cap = MAX_ADVERTISED_MCP_TOOLS,
            "skipping MCP tools past the per-session advertisement cap"
        );
    }
    (advertised, life)
}

/// Open a drive scope for a session's MCP life: fail closed on a
/// torn-down binding and — inside the same critical section — snapshot the
/// life's cancel token AND epoch. Teardown cancels that token (captured
/// under this same lock) to drop in-flight start futures, and every
/// downstream commit ([`record_bridge_outcome`], [`install_servers`],
/// `settle_registrations`) gates on the epoch — so a teardown+revive during
/// a start can neither leave the drive uncancellable nor let its outcomes
/// commit into the new life. The CALLER holds the scope so the install half
/// of a convergence gates on the same life the drive verified.
pub(crate) async fn begin_mcp_drive(
    session: &WorkspaceSession,
) -> WorkspaceResult<(tokio_util::sync::CancellationToken, u64)> {
    let binding = session.mcp_binding.lock().await;
    if matches!(*binding, WorkspaceMcpBinding::Closed) {
        return Err(WorkspaceError::SessionNotFound(session.session_id.clone()));
    }
    Ok((
        session.mcp_cancel.lock().clone(),
        session.mcp_epoch.load(std::sync::atomic::Ordering::SeqCst),
    ))
}

/// Start `configs` and send each server down `outcomes` AS IT FINISHES, all
/// bounded by a single absolute discovery deadline so one hanging server
/// cannot delay the rest — neither their starts (per-server futures run
/// concurrently) nor their publication (outcomes stream per arrival, so a
/// caller can advertise a fast server's tools while a slow sibling is still
/// discovering).
///
/// One future per server, driven directly: a server still starting when the
/// deadline fires is cancelled (its future dropped) and reported as a
/// timeout failure. Cancel safety rests on the crate conventions — child
/// processes die with their `kill_on_drop` process groups and token state
/// is written atomically — and a cancelled or failed server is absent from
/// the session map, so the next convergence retries it.
///
/// Per-server [`McpState`] bookkeeping (handshake progress, failures, owned
/// clients) happens here; updating the *config list* is the caller's job.
/// The receiver closing aborts the drive (remaining starts are cancelled)
/// after closing out the [`McpState`] init bookkeeping, as does the
/// session's teardown cancelling `mcp_cancel`.
///
/// [`McpState`]: xai_grok_mcp::servers::McpState
pub(crate) async fn drive_server_starts(
    session: &WorkspaceSession,
    session_id: &str,
    configs: Vec<agent_client_protocol::McpServer>,
    discovery_timeout: Duration,
    first_party: &HashSet<String>,
    event_writer: xai_grok_session_events::EventWriter,
    outcomes: tokio::sync::mpsc::Sender<Result<StartedMcpServer, McpStartFailure>>,
    drive_scope: (tokio_util::sync::CancellationToken, u64),
) -> WorkspaceResult<()> {
    let (cancel, life) = drive_scope;
    if configs.is_empty() {
        return Ok(());
    }
    let sid = SessionId::new(session_id)
        .map_err(|error| WorkspaceError::HubError(format!("invalid session_id: {error}")))?;

    let discovery_deadline = tokio::time::Instant::now() + discovery_timeout;
    let mut remaining_names: HashSet<String> = configs
        .iter()
        .map(|config| xai_grok_mcp::servers::mcp_server_name(config).to_owned())
        .collect();
    {
        // Same binding → mcp_state nesting AND the same life comparison as
        // every other state write in this function: the scope was opened by
        // the caller (`begin_mcp_drive`), so a teardown+revive can land
        // before this block — a stale drive must not stamp the revived
        // life's init progress with its servers. (It then aborts at the
        // select below: its life's token is already cancelled.)
        let _binding = session.mcp_binding.lock().await;
        if session.mcp_epoch.load(std::sync::atomic::Ordering::SeqCst) == life {
            let mut state = session.mcp_state.lock().await;
            let _ = state.try_start_init();
            state.mark_servers_initializing(remaining_names.iter().cloned());
        }
    }
    // Per-server startup watchdog sized to the shared deadline, so a hung
    // handshake fails on its own before the deadline has to cancel it.
    let startup_timeout_sec = discovery_timeout
        .as_secs()
        .saturating_add(u64::from(discovery_timeout.subsec_nanos() != 0))
        .max(1);
    let overrides = McpClientTimeoutOverrides {
        startup_timeout_sec: Some(startup_timeout_sec),
        ..Default::default()
    };
    // Two spawn contexts, chosen PER SERVER: only first-party app endpoints
    // get the agent-id header (which carries the bound session id and flips
    // the transport to the local-agent posture — no OAuth probe, no proxy,
    // no redirects). A per-drive flag here would leak the session id to
    // every user-configured third-party server in the bind config and break
    // their auth.
    let ctx_plain = McpSpawnCtx::for_session(
        session_id,
        &event_writer,
        OauthInteractivity::Interactive,
        None,
    );
    let ctx_first_party = McpSpawnCtx::for_session(
        session_id,
        &event_writer,
        OauthInteractivity::Interactive,
        None,
    )
    .with_grok_agent_id_header();

    let mut pending: FuturesUnordered<_> = configs
        .into_iter()
        .map(|config| {
            let server_name = xai_grok_mcp::servers::mcp_server_name(&config).to_owned();
            let bridge_session_id = sid.clone();
            let overrides = &overrides;
            let ctx = if first_party.contains(&server_name) {
                &ctx_first_party
            } else {
                &ctx_plain
            };
            async move {
                let client = xai_grok_mcp::servers::start_mcp_server(
                    config,
                    Some(overrides),
                    None,
                    None,
                    ctx,
                )
                .await
                .map_err(|error| McpStartFailure {
                    name: server_name.clone(),
                    error: error.to_string(),
                })?;
                let client = Arc::new(client);
                let transport: Arc<dyn McpTransport> =
                    Arc::new(McpClientTransportAdapter::new(Arc::clone(&client)));
                let config = McpBridgeConfig {
                    session_id: bridge_session_id,
                    // The bridge namespaces every tool by server name;
                    // `claim_tools` relies on that to tell siblings'
                    // same-named tools apart.
                    namespace: Some(server_name.clone()),
                };
                let bridge = McpBridge::connect(transport, &config)
                    .await
                    .map_err(|error| McpStartFailure {
                        name: server_name.clone(),
                        error: error.to_string(),
                    })?;
                Ok::<_, McpStartFailure>((server_name, client, bridge))
            }
        })
        .collect();

    let deadline = tokio::time::sleep_until(discovery_deadline);
    tokio::pin!(deadline);
    let mut deadline_fired = false;
    let mut cancelled = false;
    let mut receiver_gone = false;
    while !pending.is_empty() {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                cancelled = true;
                break;
            }
            _ = &mut deadline => {
                deadline_fired = true;
                break;
            }
            outcome = pending.next() => {
                let Some(outcome) = outcome else { break };
                if let Some(result) =
                    record_bridge_outcome(outcome, session, &mut remaining_names, life).await
                    && outcomes.send(result).await.is_err()
                {
                    receiver_gone = true;
                    break;
                }
            }
        }
    }
    if cancelled || receiver_gone {
        // Teardown or caller abort: drop the pending starts (killing their
        // children) and report nothing further.
        drop(pending);
        finish_init_if_life(session, life).await;
        return if cancelled {
            Err(WorkspaceError::SessionNotFound(session_id.to_owned()))
        } else {
            Ok(())
        };
    }
    if deadline_fired {
        // Outcomes that are already complete keep their real result...
        while let Some(Some(outcome)) = futures::FutureExt::now_or_never(pending.next()) {
            if let Some(result) =
                record_bridge_outcome(outcome, session, &mut remaining_names, life).await
            {
                let _ = outcomes.send(result).await;
            }
        }
        // ...and stragglers are cancelled by the drop.
        drop(pending);
        for name in remaining_names.drain() {
            {
                // Life-gated like every state write: a straggler timing out
                // after a teardown+revive must not smear failures onto the
                // new life's state.
                let _binding = session.mcp_binding.lock().await;
                if session.mcp_epoch.load(std::sync::atomic::Ordering::SeqCst) == life {
                    let mut state = session.mcp_state.lock().await;
                    state.record_init_failure(
                        &name,
                        false,
                        Some(format!(
                            "MCP discovery timed out after {discovery_timeout:?}"
                        )),
                    );
                    state.mark_server_ready(&name);
                }
            }
            let _ = outcomes
                .send(Err(McpStartFailure {
                    name,
                    error: format!("MCP discovery timed out after {discovery_timeout:?}"),
                }))
                .await;
        }
    }
    finish_init_if_life(session, life).await;
    Ok(())
}

/// Close out the init-progress bookkeeping, but only if `life` is still the
/// session's current MCP life — a drive outlived by a teardown+revive must
/// not stamp the NEW life's init progress.
async fn finish_init_if_life(session: &WorkspaceSession, life: u64) {
    let _binding = session.mcp_binding.lock().await;
    if session.mcp_epoch.load(std::sync::atomic::Ordering::SeqCst) == life {
        let mut state = session.mcp_state.lock().await;
        state.finish_init();
        state.mark_all_servers_ready();
    }
}

/// Dedupe MCP server configs by name, LAST definition wins (JSON-object
/// semantics — name-keyed sources can only produce duplicates through
/// list-shaped construction). The session maps hold one slot per name;
/// without this, discovery would start one client per entry and the
/// second `install_servers` insert would drop the first live
/// [`McpBridgeHandle`] without shutdown. One helper for BOTH config
/// chokepoints — `BindMcpConfig::new` (machine-owned) and
/// `start_session_mcp_servers` (client-driven `workspace.configure_mcp`)
/// — so the two paths cannot drift.
pub(crate) fn dedupe_servers_last_wins(servers: &mut Vec<agent_client_protocol::McpServer>) {
    let mut seen = std::collections::HashSet::new();
    let mut dropped = 0usize;
    // Iterate from the back so the LAST occurrence of each name is the one
    // kept, preserving its position.
    for index in (0..servers.len()).rev() {
        let name = xai_grok_mcp::servers::mcp_server_name(&servers[index]).to_owned();
        if !seen.insert(name) {
            servers.remove(index);
            dropped += 1;
        }
    }
    if dropped > 0 {
        tracing::warn!(
            dropped,
            "MCP config has duplicate server names; keeping the last definition of each"
        );
    }
}

/// Cap a server-config list at [`crate::config::BindMcpConfig::MAX_SERVERS`],
/// keeping the first entries in config order. Shared by BOTH config
/// chokepoints — `BindMcpConfig::new` (machine-owned) and
/// `start_session_mcp_servers` (client-driven `workspace.configure_mcp`) —
/// so every entry ends up costing bounded resources (process, connection,
/// discovery work) no matter which path configured it.
pub(crate) fn cap_servers(servers: &mut Vec<agent_client_protocol::McpServer>) {
    if servers.len() > crate::config::BindMcpConfig::MAX_SERVERS {
        tracing::warn!(
            configured = servers.len(),
            cap = crate::config::BindMcpConfig::MAX_SERVERS,
            "MCP config exceeds the server cap; keeping the first entries in config order"
        );
        servers.truncate(crate::config::BindMcpConfig::MAX_SERVERS);
    }
}

/// [`drive_server_starts`], collected into one batch result: the
/// `workspace.configure_mcp` RPC path, whose wire contract wants the full
/// started/failed report at once.
pub(crate) async fn connect_servers(
    session: &WorkspaceSession,
    session_id: &str,
    configs: Vec<agent_client_protocol::McpServer>,
    discovery_timeout: Duration,
    first_party: &HashSet<String>,
    event_writer: xai_grok_session_events::EventWriter,
) -> WorkspaceResult<(StartedMcp, u64)> {
    let drive_scope = begin_mcp_drive(session).await?;
    let life = drive_scope.1;
    // One outcome per config entry, and the entry count is capped at
    // MAX_SERVERS by both config chokepoints — so this named capacity can
    // never fill and a send never blocks.
    let (tx, mut rx) = tokio::sync::mpsc::channel(crate::config::BindMcpConfig::MAX_SERVERS);
    let drive = drive_server_starts(
        session,
        session_id,
        configs,
        discovery_timeout,
        first_party,
        event_writer,
        tx,
        drive_scope,
    );
    let collect = async {
        let mut servers = Vec::new();
        let mut failed = Vec::new();
        while let Some(outcome) = rx.recv().await {
            match outcome {
                Ok(server) => servers.push(server),
                Err(failure) => failed.push(failure),
            }
        }
        (servers, failed)
    };
    let (driven, (servers, failed)) = tokio::join!(drive, collect);
    driven?;
    tracing::info!(
        session_id,
        started = servers.len(),
        failed = failed.len(),
        "MCP servers connected"
    );
    Ok((StartedMcp { servers, failed }, life))
}

/// Add `started` to the session's server map.
///
/// Enrols an `Uninitialized` session, which is how a session joins the
/// configured set. Servers arrive owning nothing; [`claim_tools`] decides
/// what each may advertise.
///
/// # Errors
///
/// [`WorkspaceError::SessionNotFound`] when the session was torn down while
/// its servers were starting. The caller drops the bridges, which closes
/// their transports.
pub(crate) async fn install_servers(
    session: &WorkspaceSession,
    started: Vec<StartedMcpServer>,
    expected_life: u64,
) -> WorkspaceResult<()> {
    let mut binding = session.mcp_binding.lock().await;
    // Life-gated, not merely state-gated: a server recorded under an older
    // life can still be IN FLIGHT through a convergence's publish channel
    // when a teardown+revive opens a new life — a bare `join()` would
    // insert it into that new life, which would then treat the stale
    // client as already running and never start a fresh one.
    if session.mcp_epoch.load(std::sync::atomic::Ordering::SeqCst) != expected_life {
        return Err(WorkspaceError::SessionNotFound(session.session_id.clone()));
    }
    let Some(active) = binding.join() else {
        return Err(WorkspaceError::SessionNotFound(session.session_id.clone()));
    };
    for server in started {
        active.servers.insert(
            server.name,
            SessionMcpServer {
                bridge: server.bridge,
                tool_ids: Vec::new(),
            },
        );
    }
    Ok(())
}

/// Stop `names` for this session: unadvertise exactly the ids each owns, then
/// shut its bridge down. Returns the servers actually stopped.
///
/// A tool call in flight against a stopped server fails when its transport
/// closes. That is deliberate — the alternative is waiting on an arbitrary
/// third-party server that may never answer, which would hang the
/// convergence.
pub(crate) async fn stop_servers(
    session: &WorkspaceSession,
    session_id: &SessionId,
    tool_server: &impl HubToolRegistry,
    names: &[String],
) -> Vec<String> {
    let (stopped, life) = {
        let mut binding = session.mcp_binding.lock().await;
        // The life whose registrations this stop removes, observed under
        // the same lock the extraction holds: a stale post-CS unregister
        // can then never hit a newer life's same-id registration.
        let life = session.mcp_epoch.load(std::sync::atomic::Ordering::SeqCst);
        let Some(active) = binding.active_mut() else {
            return Vec::new();
        };
        let stopped: Vec<(String, SessionMcpServer)> = names
            .iter()
            .filter_map(|name| active.servers.remove_entry(name))
            .collect();
        (stopped, life)
    };
    let mut names = Vec::with_capacity(stopped.len());
    for (name, server) in stopped {
        for tool_id in &server.tool_ids {
            if let Err(error) = tool_server
                .unregister_tool_dynamic(tool_id, session_id, life)
                .await
            {
                tracing::warn!(%session_id, tool = %tool_id, %error, "failed to unadvertise MCP tool");
            }
        }
        if let Err(error) = server.bridge.bridge.shutdown().await {
            tracing::warn!(%session_id, server = %name, %error, "MCP bridge shutdown failed");
        }
        names.push(name);
    }
    names
}

/// Install `started` and advertise its tools under the legacy qualified
/// `server__tool` names — `workspace.configure_mcp`'s wire contract. The
/// second `tool_ids` writer besides [`claim_tools`]: the qualified names
/// are pre-namespaced per server, so there is nothing to disambiguate, and
/// the two writers are mutually exclusive by construction — this RPC is
/// refused on a workspace whose servers come from local configuration.
pub(crate) async fn install_and_advertise_qualified(
    session: &WorkspaceSession,
    session_id: &SessionId,
    tool_server: &impl HubToolRegistry,
    started: Vec<StartedMcpServer>,
    life: u64,
) -> WorkspaceResult<()> {
    // Same per-session advertisement cap as the bind path's `claim_tools`:
    // the legacy path REPLACES the session's servers (`stop_servers` ran),
    // so the count starts from zero. First tools in server order win.
    let mut total = 0usize;
    let mut over_cap = 0usize;
    let advertised: Vec<(String, Vec<Arc<dyn ToolServerHandler>>)> = started
        .iter()
        .map(|server| {
            let handlers = server
                .bridge
                .bridge
                .handlers()
                .iter()
                .filter_map(|handler| {
                    QualifiedMcpToolHandler::from_namespaced(handler.clone())
                        .map(|qualified| Arc::new(qualified) as Arc<dyn ToolServerHandler>)
                })
                .filter(|_| {
                    if total >= MAX_ADVERTISED_MCP_TOOLS {
                        over_cap += 1;
                        false
                    } else {
                        total += 1;
                        true
                    }
                })
                .collect();
            (server.name.clone(), handlers)
        })
        .collect();
    if over_cap > 0 {
        tracing::warn!(
            over_cap,
            cap = MAX_ADVERTISED_MCP_TOOLS,
            "skipping MCP tools past the per-session advertisement cap"
        );
    }
    install_servers(session, started, life).await?;
    for (name, handlers) in advertised {
        let mut owned = Vec::new();
        for handler in handlers {
            let tool_id = handler.tool_id();
            match tool_server
                .register_tool_dynamic(handler, vec![session_id.clone()], life)
                .await
            {
                Ok(()) => owned.push(tool_id),
                Err(error) => tracing::warn!(
                    %session_id,
                    tool = %tool_id,
                    %error,
                    "failed to register MCP tool on hub"
                ),
            }
        }
        // Records this server's contribution, or — when a teardown
        // interleaved with the registrations above — unregisters them again
        // so the hub does not keep routing into dropped bridges.
        let recorded = owned.clone();
        settle_registrations(
            session,
            session_id,
            tool_server,
            owned,
            life,
            move |active| {
                if let Some(server) = active.servers.get_mut(&name) {
                    server.tool_ids = recorded;
                }
            },
        )
        .await;
    }
    Ok(())
}

/// What converging one session onto a new configuration changed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SessionMcpDelta {
    /// Servers started and advertised.
    pub added: Vec<String>,
    /// Servers stopped and unadvertised.
    pub removed: Vec<String>,
    /// Servers the convergence tried to start but could not.
    pub failed: Vec<String>,
}

impl SessionMcpDelta {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.failed.is_empty()
    }
}

/// Whether a convergence with an unchanged server plan still re-claims tool
/// ownership and reconciles the hub.
///
/// A reload converges every session and skips the untouched ones
/// (`IfChanged`); a bind must reconcile even when no server starts or
/// stops, because its *native* tool set may have changed and a claimed MCP
/// id could newly collide with it (`Always`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum McpReclaim {
    IfChanged,
    Always,
}

/// Which of a session's servers must stop, and which must start, to match a
/// configuration.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ConvergencePlan {
    pub(crate) stop: Vec<String>,
    pub(crate) start: Vec<String>,
}

/// Decide the plan from what a session runs (`live`), what the configuration
/// asks for (`wanted`, in config order), and which names the configuration
/// redefined (`redefined`).
///
/// A running server stops when the configuration dropped it or changed its
/// definition. Anything wanted that is then not running starts — which covers
/// new servers, redefined ones just stopped, and servers whose previous start
/// failed, since a failed server never entered `live`. Untouched servers
/// appear in neither list and keep running.
pub(crate) fn plan_convergence(
    live: &HashSet<String>,
    wanted: &[String],
    redefined: &HashSet<String>,
) -> ConvergencePlan {
    let keeps = |name: &String| wanted.contains(name) && !redefined.contains(name);
    let stop: Vec<String> = live.iter().filter(|name| !keeps(name)).cloned().collect();
    let start = wanted
        .iter()
        .filter(|name| !live.contains(*name) || !keeps(name))
        .cloned()
        .collect();
    ConvergencePlan { stop, start }
}

/// Make one session's MCP servers match `desired`.
///
/// Stops servers the config dropped or redefined, starts everything it wants
/// that the session is not already running, and leaves untouched servers
/// alone — their client, process, and in-flight calls all survive. A server
/// whose previous start failed is absent from the session's map, so any
/// convergence — bind-spawned or reload-driven — is also its retry. Any
/// change to the server set re-claims tool ids and reconciles the hub with
/// the outcome: an id a surviving server lost to a new collision is
/// unadvertised, and one freed by a removal is advertised again.
///
/// No-op for sessions that never joined the configured set. Callers hold the
/// session's `update_lock`, which is what keeps this from interleaving with
/// a bind or another reload. Teardown deliberately holds no lock and instead
/// flips the binding to `Closed`, which every step here fails closed
/// against: [`install_servers`] refuses the session, [`claim_tools`] yields
/// nothing, and teardown's `mcp_cancel` drops any starts still in flight.
pub(crate) async fn converge_session(
    session: &WorkspaceSession,
    session_id: &str,
    desired: &crate::config::BindMcpConfig,
    tool_server: &impl HubToolRegistry,
    reclaim: McpReclaim,
    event_writer: xai_grok_session_events::EventWriter,
) -> WorkspaceResult<SessionMcpDelta> {
    let sid = SessionId::new(session_id)
        .map_err(|error| WorkspaceError::HubError(format!("invalid session_id: {error}")))?;
    let Some(live) = session
        .mcp_binding
        .lock()
        .await
        .active()
        .map(|active| active.servers.keys().cloned().collect::<HashSet<_>>())
    else {
        return Ok(SessionMcpDelta::default());
    };

    let wanted: Vec<String> = desired
        .servers()
        .iter()
        .map(|config| xai_grok_mcp::servers::mcp_server_name(config).to_owned())
        .collect();
    // The diff keeps unchanged servers' clients alive and forgets the rest.
    // Its `added` doubles as "redefined" for names already running.
    let redefined: HashSet<String> = {
        let mut state = session.mcp_state.lock().await;
        state
            .update_configs_diff(desired.servers().to_vec())
            .map(|diff| diff.added.into_iter().collect())
            .unwrap_or_default()
    };

    let plan = plan_convergence(&live, &wanted, &redefined);
    if plan == ConvergencePlan::default() && reclaim == McpReclaim::IfChanged {
        return Ok(SessionMcpDelta::default());
    }
    let mut delta = SessionMcpDelta {
        removed: stop_servers(session, &sid, tool_server, &plan.stop).await,
        ..Default::default()
    };
    // The bind recorded which ids are native; the hub snapshot cannot be
    // used for this, because it also holds already-advertised MCP tools and
    // counting those as taken would make a surviving server lose the very
    // ids it is serving.
    let native = session.mcp_native_tool_ids.lock().clone();
    if !plan.start.is_empty() {
        let starting: HashSet<&str> = plan.start.iter().map(String::as_str).collect();
        let configs = desired
            .servers()
            .iter()
            .filter(|config| starting.contains(xai_grok_mcp::servers::mcp_server_name(config)))
            .cloned()
            .collect();
        let drive_scope = begin_mcp_drive(session).await?;
        let drive_life = drive_scope.1;
        // Same bound as `connect_servers`: one outcome per (capped) entry.
        let (tx, mut rx) = tokio::sync::mpsc::channel(crate::config::BindMcpConfig::MAX_SERVERS);
        let drive = drive_server_starts(
            session,
            session_id,
            configs,
            desired.discovery_timeout(),
            desired.first_party_servers(),
            event_writer,
            tx,
            drive_scope,
        );
        // Publish EACH server's tools as its discovery completes, so one
        // slow or hanging server never delays a healthy sibling's tools
        // (the drive's deadline only bounds the stragglers). Dropping the
        // receiver on an install failure aborts the drive.
        let publish = async {
            while let Some(outcome) = rx.recv().await {
                match outcome {
                    Ok(server) => {
                        // Life-gated on the DRIVE's life: the outcome was
                        // recorded under it, but this publish half runs
                        // concurrently — a teardown+revive between the
                        // record and this install must refuse the stale
                        // server, or the revived life would treat it as
                        // already running.
                        let name = server.name.clone();
                        install_servers(session, vec![server], drive_life).await?;
                        delta.added.push(name);
                        reconcile_session_tools(session, &sid, tool_server, &native).await;
                    }
                    Err(failure) => delta.failed.push(failure.name),
                }
            }
            Ok::<(), WorkspaceError>(())
        };
        let (driven, published) = tokio::join!(drive, publish);
        driven?;
        published?;
    }
    // Reconciliation also runs when the convergence started nothing: stopping a
    // clashing server frees its tool id for a survivor to re-claim.
    reconcile_session_tools(session, &sid, tool_server, &native).await;
    Ok(delta)
}

/// Re-claim the session's MCP tool ids and reconcile the hub with the
/// outcome. Idempotent; runs after every server-set change.
///
/// A re-claim can take an id away from a surviving server: a just-started
/// server offering the same tool name makes the id ambiguous, and
/// `claim_tools` then drops it from both. Such an id is still registered
/// on the hub but owned by nobody, so no later removal would clean it up —
/// it has to be unadvertised here.
async fn reconcile_session_tools(
    session: &WorkspaceSession,
    sid: &SessionId,
    tool_server: &impl HubToolRegistry,
    native: &HashSet<ToolId>,
) {
    let owned_before = owned_tool_ids(session).await;
    let (claimed, life) = claim_tools(session, native).await;
    let owned_after = owned_tool_ids(session).await;
    for tool_id in owned_before.difference(&owned_after) {
        // Life-tagged: if this id is no longer a THIS-life dynamic
        // registration (a resolver-installed native now holds it, or a
        // newer life re-registered it), the unregister is a no-op rather
        // than stripping the other owner's handler.
        if let Err(error) = tool_server
            .unregister_tool_dynamic(tool_id, sid, life)
            .await
        {
            tracing::warn!(session_id = %sid, tool = %tool_id, %error, "failed to unadvertise MCP tool");
        }
    }

    let mut registered = Vec::new();
    for handler in claimed {
        let tool_id = handler.tool_id();
        // No is-it-already-on-the-hub skip: the hub's life-tagged ledger
        // makes a same-life re-register an idempotent no-op, an older
        // life's stale registration is SUPERSEDED (an is-on-hub skip here
        // would leave the id routing into the closed life's dropped
        // bridge), and a native's id was already excluded by `claim_tools`.
        match tool_server
            .register_tool_dynamic(handler, vec![sid.clone()], life)
            .await
        {
            Ok(()) => registered.push(tool_id),
            Err(error) => {
                tracing::warn!(session_id = %sid, tool = %tool_id, %error, "failed to advertise MCP tool on hub");
            }
        }
    }
    // Ownership was already recorded by `claim_tools`; the gate only has to
    // catch a teardown that landed while the ids above were registering.
    settle_registrations(session, sid, tool_server, registered, life, |_active| {}).await;
}

/// Settle hub tool registrations made outside the binding lock: either the
/// life they were claimed under is still current (run `commit` under the
/// binding lock — the qualified path records ownership there; the
/// configured path has nothing left to record, `claim_tools` already did)
/// or that life is gone (unregister the just-registered ids again). The
/// GATE is the point; the closure is only what a caller still needs made
/// atomic with it.
///
/// `expected_life` is the `mcp_epoch` the caller observed under the lock
/// when it claimed/installed (see [`claim_tools`] / [`install_servers`]):
/// a bare is-Active check would let registrations made for a closed life
/// commit into a REVIVED life — recorded on the wrong life's servers, or
/// left registered on the hub routing into dropped bridges.
///
/// Teardown flips the binding to `Closed` under `mcp_binding` and then
/// unregisters only the ids recorded on the servers it extracted — so an id
/// registered on the hub *after* that extraction would stay registered
/// forever. Every path that registers MCP tools funnels through this after
/// its registrations land.
pub(crate) async fn settle_registrations(
    session: &WorkspaceSession,
    session_id: &SessionId,
    tool_server: &impl HubToolRegistry,
    registered: Vec<ToolId>,
    expected_life: u64,
    commit: impl FnOnce(&mut crate::session::ActiveMcp),
) {
    {
        let mut binding = session.mcp_binding.lock().await;
        if session.mcp_epoch.load(std::sync::atomic::Ordering::SeqCst) == expected_life
            && let Some(active) = binding.active_mut()
        {
            commit(active);
            return;
        }
    }
    for tool_id in &registered {
        if let Err(error) = tool_server
            .unregister_tool_dynamic(tool_id, session_id, expected_life)
            .await
        {
            tracing::warn!(
                %session_id,
                tool = %tool_id,
                %error,
                "failed to unadvertise MCP tool after a teardown race"
            );
        }
    }
}

/// Every id the session's MCP servers currently own.
async fn owned_tool_ids(session: &WorkspaceSession) -> HashSet<ToolId> {
    session
        .mcp_binding
        .lock()
        .await
        .active()
        .map(|active| {
            active
                .servers
                .values()
                .flat_map(|server| server.tool_ids.iter().cloned())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestHandler;

    #[async_trait]
    impl ToolServerHandler for TestHandler {
        fn tool_id(&self) -> ToolId {
            ToolId::new("lookup").unwrap()
        }

        fn description(&self) -> ToolDescription {
            ToolDescription::new("lookup", "test").with_namespace("server")
        }

        fn input_schema(&self) -> Option<Value> {
            None
        }

        async fn handle_call(
            &self,
            _ctx: ToolCallContext,
            _args: Value,
        ) -> ToolStream<TypedToolOutput> {
            panic!("constructor test does not call the tool")
        }
    }

    #[test]
    fn legacy_handler_uses_qualified_name() {
        let handler = QualifiedMcpToolHandler::from_namespaced(Arc::new(TestHandler)).unwrap();
        assert_eq!(handler.tool_id().as_str(), "server__lookup");
        assert_eq!(handler.description().name, "server__lookup");
    }

    /// The client-driven configure path honors the SAME server cap as the
    /// machine-owned chokepoint — `cap_servers` is the one helper both run,
    /// keeping the first entries in config order (matching
    /// `BindMcpConfig::new`'s documented cap semantics).
    #[test]
    fn cap_servers_keeps_the_first_entries_in_config_order() {
        let http = |name: &str| {
            agent_client_protocol::McpServer::Http(
                agent_client_protocol::McpServerHttp::new(name, "http://cap.invalid/mcp")
                    .headers(vec![]),
            )
        };
        let over = crate::config::BindMcpConfig::MAX_SERVERS + 5;
        let mut servers: Vec<_> = (0..over).map(|i| http(&format!("server-{i:03}"))).collect();
        cap_servers(&mut servers);
        assert_eq!(crate::config::BindMcpConfig::MAX_SERVERS, servers.len());
        assert_eq!(
            format!(
                "server-{:03}",
                crate::config::BindMcpConfig::MAX_SERVERS - 1
            ),
            xai_grok_mcp::servers::mcp_server_name(
                &servers[crate::config::BindMcpConfig::MAX_SERVERS - 1]
            ),
        );
    }

    /// The shared dedupe both config chokepoints run — `BindMcpConfig::new`
    /// AND the client-driven `start_session_mcp_servers` list before
    /// `connect_servers`: one slot per name, LAST definition wins at its
    /// position, so a duplicate can never start two clients and drop the
    /// first bridge handle without shutdown.
    #[test]
    fn dedupe_servers_last_wins_keeps_last_definition_in_place() {
        let http = |name: &str, url: &str| {
            agent_client_protocol::McpServer::Http(
                agent_client_protocol::McpServerHttp::new(name, url).headers(vec![]),
            )
        };
        let mut servers = vec![
            http("dup", "http://first.invalid/mcp"),
            http("other", "http://other.invalid/mcp"),
            http("dup", "http://last.invalid/mcp"),
        ];
        dedupe_servers_last_wins(&mut servers);
        let named: Vec<(&str, &str)> = servers
            .iter()
            .map(|server| {
                let agent_client_protocol::McpServer::Http(http) = server else {
                    panic!("fixture builds http servers only");
                };
                (
                    xai_grok_mcp::servers::mcp_server_name(server),
                    http.url.as_str(),
                )
            })
            .collect();
        assert_eq!(
            named,
            [
                ("other", "http://other.invalid/mcp"),
                ("dup", "http://last.invalid/mcp"),
            ],
            "one slot per name, the LAST definition kept at its position"
        );
    }

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    fn name_set(values: &[&str]) -> HashSet<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    #[test]
    fn plan_adds_without_disturbing_running_servers() {
        let plan = plan_convergence(
            &name_set(&["figma"]),
            &names(&["figma", "linear"]),
            &HashSet::new(),
        );
        assert_eq!(
            plan,
            ConvergencePlan {
                stop: Vec::new(),
                start: names(&["linear"]),
            }
        );
    }

    #[test]
    fn plan_stops_servers_the_config_dropped() {
        let plan = plan_convergence(
            &name_set(&["figma", "linear"]),
            &names(&["figma"]),
            &HashSet::new(),
        );
        assert_eq!(
            plan,
            ConvergencePlan {
                stop: names(&["linear"]),
                start: Vec::new(),
            }
        );
    }

    /// A redefined server is restarted, not left running on its old config.
    #[test]
    fn plan_restarts_a_redefined_server() {
        let plan = plan_convergence(
            &name_set(&["figma"]),
            &names(&["figma"]),
            &name_set(&["figma"]),
        );
        assert_eq!(
            plan,
            ConvergencePlan {
                stop: names(&["figma"]),
                start: names(&["figma"]),
            }
        );
    }

    /// A server whose start failed is not in `live`, so the next reload
    /// retries it. Without this, a server that was down when the app booted
    /// would stay dead until a restart.
    #[test]
    fn plan_retries_a_server_that_failed_to_start() {
        let plan = plan_convergence(
            &name_set(&["figma"]),
            &names(&["figma", "flaky"]),
            &HashSet::new(),
        );
        assert_eq!(plan.stop, Vec::<String>::new());
        assert_eq!(plan.start, names(&["flaky"]));
    }

    #[test]
    fn plan_for_an_unchanged_config_is_empty() {
        let plan = plan_convergence(
            &name_set(&["figma", "linear"]),
            &names(&["linear", "figma"]),
            &HashSet::new(),
        );
        assert_eq!(plan, ConvergencePlan::default());
    }
}
