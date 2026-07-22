//! MCP server integration using the official rmcp SDK.

use std::collections::HashMap;
use std::ffi::OsString;
use std::future::Future;
use std::sync::{Arc, LazyLock};

use agent_client_protocol as acp;
use futures::StreamExt;
use regex::Regex;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    process::{ChildStderr, Command},
    sync::{Mutex, Notify},
};

use rmcp::{
    ClientHandler, ServiceExt,
    model::{
        CallToolRequestParams, ClientCapabilities, ClientInfo, Implementation,
        PaginatedRequestParams,
    },
    service::{
        ClientInitializeError, NotificationContext, RoleClient, RunningService, ServiceError,
    },
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::{
        StreamableHttpClientTransport, Transport,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};

use crate::oauth_config::McpOAuthConfig;

use xai_grok_tools::types::{
    output::{MCPOutput, MCPOutputDetails, ToolOutput},
    tool::{ToolKind, ToolNamespace},
    tool_metadata::ToolMetadata,
};
use xai_grok_tools::util::ProcessGroup;

/// MCP tool name delimiter: server names are qualified as `"server__tool"`.
/// Canonical definition lives in `xai_grok_workspace_types`; re-exported here
/// for callers that historically imported it from this module.
pub use xai_grok_workspace_types::MCP_TOOL_NAME_DELIMITER;

/// Normalize an MCP server URL for comparison: strip trailing slashes.
/// Must match the normalization the host's managed-config layer uses
/// (e.g. shell's `session::managed_mcp::normalize_url`) so refresh
/// lookup keys agree.
fn normalize_url(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

/// Regex for strictest cross-provider tool name validation.
///
/// Requirements across providers:
/// - Anthropic/OpenAI: `^[a-zA-Z0-9_-]{1,64}$` (allows starting with digit/hyphen)
/// - Google Gemini: `^[a-zA-Z_][a-zA-Z0-9_.-]{0,63}$` (must start with letter/underscore, allows dots)
///
/// Strictest common denominator: must start with letter/underscore, only alphanumeric/_/- allowed, max 64 chars.
static TOOL_NAME_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z_][a-zA-Z0-9_-]{0,63}$").unwrap());

/// Validate that a tool name matches the strictest cross-provider LLM API requirements.
///
/// Pattern: `^[a-zA-Z_][a-zA-Z0-9_-]{0,63}$`
/// - Must start with a letter or underscore (Gemini requirement)
/// - Only letters, digits, underscores, hyphens allowed (no dots — Anthropic/OpenAI requirement)
/// - Maximum 64 characters
///
/// Returns `Ok(())` if valid, or `Err(reason)` if invalid.
pub fn validate_tool_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("tool name cannot be empty".to_string());
    }
    if !TOOL_NAME_REGEX.is_match(name) {
        return Err(format!(
            "tool name '{}' is invalid — must match ^[a-zA-Z_][a-zA-Z0-9_-]{{0,63}}$ (start with letter/underscore, max 64 chars)",
            name
        ));
    }
    Ok(())
}

/// Sanitize an MCP server or tool name into a single safe path segment
/// (e.g. `"user-Hugging Face"` becomes `user-Hugging_Face`). Shared so the
/// per-server folder advertised in the prompt matches the tool files on disk.
pub fn sanitize_descriptor_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Result of a diff-based MCP config update.
#[derive(Debug)]
pub struct McpConfigDiff {
    /// Server names that are new or had their config changed.
    pub added: Vec<McpServerName>,
    /// Server names that were removed or had their config changed (old instance torn down).
    pub removed: Vec<McpServerName>,
    /// Server names whose config is identical — clients kept alive.
    pub retained: Vec<McpServerName>,
}

/// MCP server name used as the key in client/tool maps (e.g. `"github"`, `"grok_com_linear"`).
pub type McpServerName = String;

/// Unqualified MCP tool name (e.g. `"create_issue"`, without the `server__` prefix).
type ToolName = String;

/// Typed state machine for MCP-pool initialization.
///
/// Replaces the previous trio of correlated fields — `initialized: bool`,
/// `initializing: bool`, `initializing_servers: HashSet<McpServerName>` —
/// whose product space could represent nonsensical combinations such as
/// "initialized AND initializing" or "no init started AND per-server
/// handshakes outstanding". With one enum field, every legal state has
/// exactly one representation and the compiler enforces exhaustiveness
/// at every match site.
///
/// Lifecycle:
///
/// ```text
///   ┌─────────────┐  try_start  ┌──────────────────────┐
///   │  NotStarted │ ──────────▶ │  Starting{handshakes}│
///   └─────────────┘ ◀── cancel ─┴──────────┬───────────┘
///         ▲                                │ finish
///         │ cancel                         ▼
///         │                  ┌──────────────────────────┐
///         └──────────────────┤  Finished{handshakes}    │
///                            └──────────────────────────┘
/// ```
///
/// `Starting` is the pre-`finish_init` window; `Finished` is the post-
/// `finish_init` window where per-server background handshakes may still
/// be draining. `is_complete()` requires `Finished` with an empty
/// handshaking set.
#[derive(Debug, Default)]
pub enum InitProgress {
    /// Init has never been started, or was cancelled / reset by a
    /// config change.
    #[default]
    NotStarted,
    /// `try_start_init` was called; per-server tasks may be spawning;
    /// `finish_init` has NOT yet fired. `handshaking` tracks the set of
    /// servers whose background handshake is in flight.
    Starting {
        handshaking: std::collections::HashSet<McpServerName>,
    },
    /// `finish_init` fired (deliberately early, so the session is not
    /// blocked on MCP for non-MCP work). Background per-server
    /// handshakes may still be running; `handshaking` shrinks as each
    /// completes. `is_complete()` returns `true` only when it is empty.
    Finished {
        handshaking: std::collections::HashSet<McpServerName>,
    },
}

impl InitProgress {
    /// True iff every per-server handshake has settled and `finish_init`
    /// has fired. Pairs with [`Self::is_in_progress`].
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Finished { handshaking } if handshaking.is_empty())
    }

    /// True iff any init work is outstanding — either we are pre-
    /// `finish_init`, or per-server handshakes are still in flight in
    /// the background.
    pub fn is_in_progress(&self) -> bool {
        match self {
            Self::Starting { .. } => true,
            Self::Finished { handshaking } => !handshaking.is_empty(),
            Self::NotStarted => false,
        }
    }

    /// True iff `finish_init` has fired, regardless of whether
    /// background handshakes are still draining. Used for diagnostic
    /// logging where the caller wants to distinguish pre-finish from
    /// post-finish-with-bg-work.
    pub fn has_finished_init(&self) -> bool {
        matches!(self, Self::Finished { .. })
    }

    /// True iff the named server is currently handshaking.
    pub fn is_server_handshaking(&self, name: &str) -> bool {
        match self {
            Self::Starting { handshaking } | Self::Finished { handshaking } => {
                handshaking.contains(name)
            }
            Self::NotStarted => false,
        }
    }

    /// Iterate over server names whose background handshake is still
    /// in flight. Empty when [`Self::NotStarted`] or fully complete.
    pub fn handshaking_servers(&self) -> impl Iterator<Item = &McpServerName> {
        match self {
            Self::Starting { handshaking } | Self::Finished { handshaking } => {
                Some(handshaking.iter())
            }
            Self::NotStarted => None,
        }
        .into_iter()
        .flatten()
    }

    /// Number of in-flight per-server handshakes.
    pub fn handshaking_count(&self) -> usize {
        match self {
            Self::Starting { handshaking } | Self::Finished { handshaking } => handshaking.len(),
            Self::NotStarted => 0,
        }
    }

    /// Transition `NotStarted` → `Starting { ∅ }`. Returns `true` on
    /// successful transition, `false` if init was already started or
    /// finished (mirrors the pre-refactor `try_start_init` contract).
    pub fn try_start(&mut self) -> bool {
        if matches!(self, Self::NotStarted) {
            *self = Self::Starting {
                handshaking: std::collections::HashSet::new(),
            };
            true
        } else {
            false
        }
    }

    /// Transition `Starting { hs }` → `Finished { hs }`, preserving the
    /// handshaking set. No-op if already `Finished`; no-op-with-log if
    /// called from `NotStarted` (defensive — that would be a caller bug).
    pub fn finish(&mut self) {
        match self {
            Self::Starting { handshaking } => {
                // Move only the inner set, leaving the outer `&mut self`
                // ready to be reassigned without going through
                // `mem::take(self)` (which would force a `NotStarted`
                // placeholder and a redundant put-back in the
                // already-Finished arm below).
                let handshaking = std::mem::take(handshaking);
                *self = Self::Finished { handshaking };
            }
            // Idempotent: already past the finish boundary. Per-server
            // handshakes continue draining via `mark_handshake_complete`.
            Self::Finished { .. } => {}
            Self::NotStarted => {
                tracing::warn!(
                    "InitProgress::finish called from NotStarted; staying in NotStarted"
                );
            }
        }
    }

    /// Transition any state → `NotStarted`. Clears all per-server
    /// progress. Used on generation mismatch (config change racing
    /// active init) and on full reset.
    pub fn cancel(&mut self) {
        *self = Self::NotStarted;
    }

    /// Add names to the handshaking set. Only meaningful in `Starting`
    /// or `Finished`; warns if called from `NotStarted` (that would
    /// mean a per-server handshake started without a `try_start_init`,
    /// which is a caller bug).
    pub fn mark_handshaking(&mut self, names: impl IntoIterator<Item = McpServerName>) {
        match self {
            Self::Starting { handshaking } | Self::Finished { handshaking } => {
                handshaking.extend(names);
            }
            Self::NotStarted => {
                tracing::warn!("InitProgress::mark_handshaking called from NotStarted; ignoring");
            }
        }
    }

    /// Remove a server from the handshaking set (on success or failure
    /// of its handshake). No-op if not present or if `NotStarted`.
    pub fn mark_handshake_complete(&mut self, name: &str) {
        match self {
            Self::Starting { handshaking } | Self::Finished { handshaking } => {
                handshaking.remove(name);
            }
            Self::NotStarted => {}
        }
    }

    /// Clear the handshaking set entirely. Used by the proxy-mode and
    /// bg-handshake completion paths as a defensive sweep after the
    /// per-server `mark_handshake_complete` calls — ensures the set is
    /// empty before/after `finish_init` fires.
    pub fn clear_handshaking(&mut self) {
        match self {
            Self::Starting { handshaking } | Self::Finished { handshaking } => {
                handshaking.clear();
            }
            Self::NotStarted => {}
        }
    }
}

/// One in-process SDK MCP server registration: its tool-namespace name and the
/// SDK-side id echoed back in `x.ai/mcp/sdk_call`. A named struct (rather than a
/// `(String, String)` tuple) so callers can't transpose the two strings.
///
/// `Deserialize`d straight from a `_meta["x.ai/mcp/servers"]` entry, so the
/// `serverId` wire field name is declared (and serde-checked) exactly once here.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AcpServerEntry {
    pub name: McpServerName,
    #[serde(rename = "serverId")]
    pub server_id: String,
}

/// The session's in-process SDK MCP servers (declared via `_meta["x.ai/mcp/servers"]`,
/// reached over the ACP reverse channel), bundled with the shared reverse-RPC invoker.
/// Held as `McpState::acp_mcp: Option<_>` so the set is one atom — present together or
/// absent, never "servers without an invoker" — and survives `update_configs` clears
/// (config reloads only touch `configs`/`owned_clients`). Per-server config.toml overrides
/// are NOT cached here — they are re-resolved per init (see [`McpState::build_pending_acp_clients`]).
struct AcpMcpRegistry {
    /// Registered servers (`name -> serverId`).
    servers: Vec<AcpServerEntry>,
    /// Shared reverse-RPC invoker all these servers' tools are called through (emits
    /// `x.ai/mcp/sdk_call` over the ACP connection).
    invoker: Arc<dyn crate::acp_transport::AcpReverseInvoker>,
}

/// Consolidated MCP state behind a single lock. Generation counter detects stale inits.
pub struct McpState {
    pub configs: Vec<acp::McpServer>,
    pub meta_config_map: McpMetaConfigMap,
    /// Clients owned by this session; cleared on config changes.
    pub owned_clients: HashMap<McpServerName, Arc<McpClient>>,
    /// Clients inherited from parent via `SharedMcpPool`; never cleared by config changes.
    pub shared_clients: HashMap<McpServerName, Arc<McpClient>>,
    /// The session's in-process SDK MCP servers + their shared invoker/overrides; `None`
    /// when the session has none. See [`AcpMcpRegistry`]. Kept out of `configs` (the closed
    /// `acp::McpServer` enum) so it survives `update_configs` clears.
    acp_mcp: Option<AcpMcpRegistry>,
    /// Encapsulated init lifecycle. Access via [`Self::is_initialized`],
    /// [`Self::is_initializing`], [`Self::try_start_init`],
    /// [`Self::finish_init`], etc. — those route through a single
    /// [`InitProgress`] state machine that rules out nonsensical
    /// combinations like "initialized AND initializing".
    ///
    /// Private on purpose: external callers must go through the typed
    /// transition methods, not poke the variant directly.
    init_progress: InitProgress,
    pub generation: u64,
    /// Qualified tool name → `_meta` from MCP tools/list. Populated during init.
    pub mcp_tool_meta: HashMap<String, serde_json::Value>,
    /// HTTP servers that support OAuth but haven't been authenticated yet.
    pub auth_required: std::collections::HashSet<McpServerName>,
    /// Servers whose background init failed (handshake error, `tools/list`
    /// error, or overall init timeout) even though a client object exists,
    /// mapped to a short failure cause surfaced to the model in the MCP
    /// reminder. Surfaced as `Unavailable` in status snapshots so a server
    /// that connected but never finished initializing — e.g. wedged on
    /// `tools/list` and registered zero tools — does not misleadingly show
    /// as `Ready`. Cleared when the server begins a fresh init attempt.
    pub init_failed: std::collections::HashMap<McpServerName, String>,
    /// Per-server set of unqualified tool names that the user has disabled.
    /// Persisted to `~/.grok/config.toml` under `[mcp_servers.<name>].disabled_tools`.
    pub disabled_tools: HashMap<McpServerName, std::collections::HashSet<ToolName>>,
    /// Stashed registrations for disabled tools so they can be re-enabled
    /// without a full MCP re-init (no need to call `list_tools` again).
    pub disabled_tool_registrations: HashMap<String, McpToolRegistration>,
    event_writer: xai_file_utils::events::EventWriter,
    /// Sender wired by the session actor to its `StatusDispatcher`
    /// task.  When `Some`, the state — and every [`McpClient`] reached
    /// through [`Self::all_clients`] / [`Self::get_client`] — forwards
    /// [`McpClientEvent`]s here for coalescing and fan-out as ACP
    /// `x.ai/mcp/server_status` notifications.
    ///
    /// Intentionally `None` in subagent-pool / shared-pool snapshots
    /// ([`SharedMcpPool`]) where the **parent** session is the
    /// single owner of liveness/notification flow. Clients in those
    /// snapshots inherit the parent's `Arc<McpClient>` (with the
    /// parent's `notify_tx` slot still pointing at the parent), so
    /// duplicating event flow into a subagent would just double-push
    /// every event.
    ///
    /// Populated by [`Self::set_client_event_tx`], which fans the
    /// sender into every existing client's `notify_tx` slot.
    ///
    /// **Private on purpose.** Callers MUST go through
    /// [`Self::set_client_event_tx`] so the sender is fanned out into
    /// every existing `owned_clients` entry; a direct field write
    /// (`state.client_event_tx = Some(tx)`) would leave all
    /// already-owned clients with `notify_tx = None`, silently
    /// dropping `tools/list_changed`, `Ready`, and `HandshakeFailed`
    /// emits for them. Read access is via [`Self::client_event_tx`].
    client_event_tx: Option<tokio::sync::mpsc::UnboundedSender<McpClientEvent>>,
}

impl McpState {
    pub fn new(configs: Vec<acp::McpServer>) -> Self {
        Self::new_with_meta(configs, McpMetaConfigMap::new())
    }

    pub fn new_with_meta(configs: Vec<acp::McpServer>, meta_config_map: McpMetaConfigMap) -> Self {
        Self {
            configs,
            meta_config_map,
            owned_clients: HashMap::new(),
            shared_clients: HashMap::new(),
            acp_mcp: None,
            init_progress: InitProgress::default(),
            generation: 0,
            mcp_tool_meta: HashMap::new(),
            auth_required: std::collections::HashSet::new(),
            init_failed: HashMap::new(),
            disabled_tools: HashMap::new(),
            disabled_tool_registrations: HashMap::new(),
            event_writer: xai_file_utils::events::EventWriter::noop(),
            client_event_tx: None,
        }
    }

    /// Install (or remove) the [`McpClientEvent`] sender owned by the
    /// session-actor `StatusDispatcher`.
    ///
    /// Synchronous: the per-client slot is a `parking_lot::Mutex`, so
    /// the iteration no longer holds `&mut McpState` across `.await`.
    ///
    /// Side effect: clones the sender into every existing client's
    /// shared `notify_tx` slot. New clients added later (e.g. on a
    /// config diff that re-spawns a server) MUST be wired by the
    /// caller post-construction — typically by calling
    /// [`McpClient::set_event_tx`] **before**
    /// `get_tool_registrations` (so `ensure_initialized`'s
    /// `Ready`/`HandshakeFailed` emit fires with `Some(tx)` and the
    /// `GrokClientHandler` cloned during `try_handshake` reads
    /// through the same Arc).
    pub fn set_client_event_tx(
        &mut self,
        tx: Option<tokio::sync::mpsc::UnboundedSender<McpClientEvent>>,
    ) {
        self.client_event_tx = tx.clone();
        for client in self.owned_clients.values() {
            client.set_event_tx(tx.clone());
        }
        // Shared clients are intentionally NOT wired here: see the
        // `client_event_tx` doc-comment for why a subagent must not
        // duplicate the parent's event flow.
    }

    /// Read-only access to the installed [`McpClientEvent`] sender.
    ///
    /// Returns a clone of the sender wired by
    /// [`Self::set_client_event_tx`], or `None` if no dispatcher is
    /// attached (subagent / shared-pool snapshot). Exposed as a getter
    /// rather than a `pub` field so the fan-out contract documented on
    /// `client_event_tx` cannot be bypassed by a direct assignment.
    pub fn client_event_tx(&self) -> Option<tokio::sync::mpsc::UnboundedSender<McpClientEvent>> {
        self.client_event_tx.clone()
    }

    pub fn set_event_writer(&mut self, writer: xai_file_utils::events::EventWriter) {
        self.event_writer = writer;
    }

    pub fn event_writer(&self) -> &xai_file_utils::events::EventWriter {
        &self.event_writer
    }

    /// Register the session's in-process SDK MCP servers (`name -> serverId`) plus the
    /// reverse-RPC invoker. Held across `update_configs` clears so each init re-adds them.
    pub fn set_acp_servers(
        &mut self,
        servers: Vec<AcpServerEntry>,
        invoker: Arc<dyn crate::acp_transport::AcpReverseInvoker>,
    ) {
        self.acp_mcp = Some(AcpMcpRegistry { servers, invoker });
    }

    /// Whether any in-process SDK MCP servers are registered (so the session knows to
    /// run MCP init even with no `configs`).
    pub fn has_acp_servers(&self) -> bool {
        self.acp_mcp
            .as_ref()
            .is_some_and(|acp| !acp.servers.is_empty())
    }

    /// Registered SDK servers not yet connected (no owned/shared client) — the ones an
    /// init pass should build. Shared by [`build_pending_acp_clients`] and
    /// [`pending_acp_server_names`] so the "what to build" filter lives in one place.
    fn pending_acp_entries(&self) -> impl Iterator<Item = &AcpServerEntry> {
        self.acp_mcp.iter().flat_map(|acp| {
            acp.servers.iter().filter(|entry| {
                !self.owned_clients.contains_key(&entry.name)
                    && !self.shared_clients.contains_key(&entry.name)
            })
        })
    }

    /// Names of the SDK servers [`build_pending_acp_clients`] will build — used to mark
    /// them initializing before the (async) build.
    pub fn pending_acp_server_names(&self) -> Vec<String> {
        self.pending_acp_entries()
            .map(|entry| entry.name.to_string())
            .collect()
    }

    /// Build [`McpClient`]s for registered ACP servers not already connected. Appended to the
    /// init handshake batch so they register tools + land in `owned_clients` on the SAME path
    /// as HTTP/stdio servers.
    ///
    /// `overrides` is the per-server config.toml tuning (keyed by server name), resolved by
    /// the caller per init — kept caller-side so this method stays pure (no file I/O under
    /// the `McpState` lock).
    pub fn build_pending_acp_clients(
        &self,
        overrides: &HashMap<String, McpClientTimeoutOverrides>,
    ) -> Vec<McpClient> {
        let Some(acp) = &self.acp_mcp else {
            return Vec::new();
        };
        self.pending_acp_entries()
            .map(|entry| {
                McpClient::new_acp(
                    entry.name.clone(),
                    entry.server_id.clone(),
                    acp.invoker.clone(),
                    overrides.get(&entry.name),
                    self.meta_config_map.get(&entry.name),
                )
            })
            .collect()
    }

    /// Check if a specific tool is disabled for a server (unqualified tool name).
    pub fn is_tool_disabled(&self, server_name: &str, tool_name: &str) -> bool {
        self.disabled_tools
            .get(server_name)
            .is_some_and(|set| set.contains(tool_name))
    }

    /// Update configs and reset initialization state.
    /// Returns true if the configs actually changed, false if they were identical.
    pub fn update_configs(&mut self, new_configs: Vec<acp::McpServer>) -> bool {
        if mcp_servers_equal(&self.configs, &new_configs) {
            tracing::debug!("MCP configs unchanged, skipping update");
            return false;
        }

        // Clear owned clients only — shared (inherited) clients are untouched.
        self.owned_clients.clear();
        self.mcp_tool_meta.clear();
        self.disabled_tool_registrations.clear();
        self.configs = new_configs;
        self.init_progress.cancel();
        self.auth_required.clear();
        self.generation = self.generation.wrapping_add(1);
        true
    }

    /// Diff-based config update: only tears down servers whose config changed
    /// or were removed, keeps healthy unchanged servers alive.
    ///
    /// Returns `None` if configs are identical (no work needed), or `Some(diff)`
    /// describing which servers to add/remove.
    pub fn update_configs_diff(
        &mut self,
        new_configs: Vec<acp::McpServer>,
    ) -> Option<McpConfigDiff> {
        if mcp_servers_equal(&self.configs, &new_configs) {
            tracing::debug!("MCP configs unchanged, skipping update");
            return None;
        }

        let old_by_name: HashMap<&str, String> = self
            .configs
            .iter()
            .filter_map(|c| match serde_json::to_string(c) {
                Ok(json) => Some((mcp_server_name(c), json)),
                Err(e) => {
                    tracing::warn!(server = mcp_server_name(c), error = %e, "Failed to serialize MCP server config for diff");
                    None
                }
            })
            .collect();

        let new_by_name: HashMap<&str, String> = new_configs
            .iter()
            .filter_map(|c| match serde_json::to_string(c) {
                Ok(json) => Some((mcp_server_name(c), json)),
                Err(e) => {
                    tracing::warn!(server = mcp_server_name(c), error = %e, "Failed to serialize MCP server config for diff");
                    None
                }
            })
            .collect();

        let mut removed = Vec::new();
        let mut added = Vec::new();
        let mut retained = Vec::new();

        for (name, old_json) in &old_by_name {
            match new_by_name.get(name) {
                None => removed.push(name.to_string()),
                Some(new_json) if new_json != old_json => {
                    removed.push(name.to_string());
                }
                Some(_) => retained.push(name.to_string()),
            }
        }

        for (name, new_json) in &new_by_name {
            match old_by_name.get(name) {
                None => added.push(name.to_string()),
                Some(old_json) if old_json != new_json => {
                    added.push(name.to_string());
                }
                Some(_) => {}
            }
        }

        for name in &removed {
            self.owned_clients.remove(name);
            self.auth_required.remove(name);
            self.init_progress.mark_handshake_complete(name);
            let prefix = format!("{}{}", name, MCP_TOOL_NAME_DELIMITER);
            self.mcp_tool_meta.retain(|k, _| !k.starts_with(&prefix));
            self.disabled_tool_registrations
                .retain(|k, _| !k.starts_with(&prefix));
        }

        tracing::info!(
            retained = retained.len(),
            added = added.len(),
            removed = removed.len(),
            "MCP config diff: {} retained, {} added, {} removed",
            retained.len(),
            added.len(),
            removed.len(),
        );

        self.configs = new_configs;
        self.init_progress.cancel();
        self.generation = self.generation.wrapping_add(1);

        Some(McpConfigDiff {
            added,
            removed,
            retained,
        })
    }

    /// Returns `true` only when MCP setup is fully complete: the
    /// init lifecycle reached [`InitProgress::Finished`] AND every
    /// per-server background handshake has settled (success or failure).
    ///
    /// The strict per-server check matters because session actors call
    /// [`Self::finish_init`] **early** (right after spawning processes,
    /// before any handshake completes) so the session isn't blocked on
    /// MCP for non-MCP work. Callers that gate MCP-tool dispatch on
    /// "is MCP actually ready" — e.g. the Blocking-strategy waits in
    /// `prepare_tool_definitions_timed`, `wait_for_mcp_initialized`,
    /// and the tool-dispatch fast path — therefore need the *combined*
    /// check or they'd race the in-flight per-server handshakes and the
    /// first tool call would land inside the
    /// [`ClientState::Initializing`] window.
    ///
    /// Delegates to [`InitProgress::is_complete`]; see that doc for the
    /// full state machine.
    pub fn is_initialized(&self) -> bool {
        self.init_progress.is_complete()
    }

    /// Returns `true` whenever any initialization work is still
    /// outstanding: pre-`finish_init` OR at least one per-server
    /// handshake is still running in the background.
    ///
    /// Pairs with [`Self::is_initialized`]: during the window between
    /// the early [`Self::finish_init`] and the background task draining
    /// the per-server handshaking set, `is_initialized()` is still
    /// `false` (per-server work remains) AND `is_initializing()` is
    /// `true` (so wait-loops keep waiting instead of kicking off a
    /// second init).
    pub fn is_initializing(&self) -> bool {
        self.init_progress.is_in_progress()
    }

    /// Returns `true` once `finish_init` has fired, regardless of
    /// whether per-server background handshakes are still draining.
    /// Used for diagnostic logging where the caller wants to
    /// distinguish "pre-finish window" from "post-finish, bg work
    /// outstanding".
    pub fn has_finished_init(&self) -> bool {
        self.init_progress.has_finished_init()
    }

    /// Borrow the underlying [`InitProgress`] state machine, primarily
    /// for tests that want to assert against the discriminant directly.
    pub fn init_progress(&self) -> &InitProgress {
        &self.init_progress
    }

    /// Try to start initialization. Returns `true` if we transitioned
    /// from [`InitProgress::NotStarted`] to [`InitProgress::Starting`];
    /// returns `false` if init is already in progress or finished.
    pub fn try_start_init(&mut self) -> bool {
        self.init_progress.try_start()
    }

    /// Transition [`InitProgress::Starting`] → [`InitProgress::Finished`],
    /// preserving the per-server handshaking set. Called early (before
    /// per-server handshakes complete) so the session is unblocked for
    /// non-MCP work — `is_initialized()` still returns `false` until
    /// every handshake has reported via [`Self::mark_server_ready`].
    pub fn finish_init(&mut self) {
        self.init_progress.finish();
    }

    /// Cancel initialization back to [`InitProgress::NotStarted`].
    /// Used when generation changed during init (config change races
    /// with active init) and on full reset.
    pub fn cancel_init(&mut self) {
        self.init_progress.cancel();
    }

    /// Add server names to the handshaking set. Call after filtering
    /// `configs_to_start`, before spawning per-server tasks. Only
    /// meaningful in [`InitProgress::Starting`] / [`InitProgress::Finished`];
    /// logs a warning otherwise.
    pub fn mark_servers_initializing(&mut self, names: impl IntoIterator<Item = McpServerName>) {
        let names: Vec<McpServerName> = names.into_iter().collect();
        // A fresh init attempt clears any prior failure for these servers so
        // a server that recovers on retry stops showing as `Unavailable`.
        for name in &names {
            self.init_failed.remove(name);
        }
        self.init_progress.mark_handshaking(names);
    }

    /// Remove a server from the handshaking set (on success or failure
    /// of its handshake). Safe if not present.
    pub fn mark_server_ready(&mut self, name: &str) {
        self.init_progress.mark_handshake_complete(name);
    }

    /// Record a per-server background-init failure for status reporting,
    /// routing it to the correct set so the two stay disjoint.
    ///
    /// `needs_auth` failures are owned by the auth state machine: its recovery
    /// paths (`handle_mcp_auth_trigger`, `retry_auth_required_servers`)
    /// re-handshake and clear `auth_required`. Such servers must therefore NOT
    /// also land in `init_failed`, or a server that successfully authenticates
    /// would stay reported as `Unavailable` with zero tools. Every other
    /// failure (handshake / `tools/list` error or init timeout) goes to
    /// `init_failed` so the server surfaces as `Unavailable`.
    ///
    /// `detail` is a short cause stored for non-auth failures (the value in
    /// [`Self::init_failed`]); ignored for `needs_auth`.
    pub fn record_init_failure(&mut self, name: &str, needs_auth: bool, detail: Option<String>) {
        if needs_auth {
            self.auth_required.insert(name.to_string());
        } else {
            self.init_failed
                .insert(name.to_string(), detail.unwrap_or_default());
        }
    }

    /// Clear a prior init failure for `name` (symmetric with
    /// [`Self::record_init_failure`]). Used by the reactive managed re-auth
    /// path so a server that recovers is no longer reported as `Unavailable`
    /// with a stale non-auth `detail`.
    pub fn clear_init_failed(&mut self, name: &str) {
        self.init_failed.remove(name);
    }

    /// Clear the entire handshaking set in one shot. Used by the
    /// proxy-mode "init complete" path and the bg-handshake completion
    /// path as a defensive sweep after the per-server
    /// [`Self::mark_server_ready`] calls; cheap no-op if already empty.
    pub fn mark_all_servers_ready(&mut self) {
        self.init_progress.clear_handshaking();
    }

    /// True iff the named server's handshake is still in flight.
    /// Used by status snapshots and tool-dispatch gating that need to
    /// know per-server progress without cloning the whole set.
    pub fn is_server_handshaking(&self, name: &str) -> bool {
        self.init_progress.is_server_handshaking(name)
    }

    /// Iterate over server names whose background handshake is still
    /// in flight. Empty when init has not started or has fully
    /// completed.
    pub fn handshaking_servers_iter(&self) -> impl Iterator<Item = &McpServerName> {
        self.init_progress.handshaking_servers()
    }

    /// Snapshot of the handshaking set as a cloned `HashSet`. Used by
    /// the status snapshot API where the caller wants an owned copy
    /// that survives lock release.
    pub fn handshaking_servers_cloned(&self) -> std::collections::HashSet<McpServerName> {
        self.init_progress.handshaking_servers().cloned().collect()
    }

    /// Number of in-flight per-server handshakes.
    pub fn handshaking_servers_count(&self) -> usize {
        self.init_progress.handshaking_count()
    }

    /// Get current generation (for stale check after async init)
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Replace managed MCP clients whose URL matches a fresh config entry.
    ///
    /// Caller passes `(endpoint, headers)` pairs from whatever source it uses
    /// (e.g. shell's cli-chat-proxy `ManagedMcpConfig` cache). The MCP crate
    /// stays free of the host's managed-config schema.
    ///
    /// Old `Arc<McpClient>` holders (in-flight tool calls) finish naturally;
    /// new calls look up the fresh client from the map.
    pub fn refresh_managed_clients<'a, I>(&mut self, fresh_configs: I)
    where
        I: IntoIterator<Item = (&'a str, &'a HashMap<String, String>)>,
    {
        let fresh_by_url: HashMap<String, (&'a str, &'a HashMap<String, String>)> = fresh_configs
            .into_iter()
            .map(|(endpoint, headers)| (normalize_url(endpoint), (endpoint, headers)))
            .collect();

        for (client_name, client) in &mut self.owned_clients {
            let Some(client_url) = self.configs.iter().find_map(|cfg| match cfg {
                acp::McpServer::Http(acp::McpServerHttp { name, url, .. })
                | acp::McpServer::Sse(acp::McpServerSse { name, url, .. })
                    if name == client_name =>
                {
                    Some(normalize_url(url))
                }
                _ => None,
            }) else {
                continue;
            };

            let Some(&(fresh_endpoint, fresh_headers)) = fresh_by_url.get(&client_url) else {
                continue;
            };
            if fresh_headers.is_empty() {
                continue;
            }
            // Rebuilding drops the warm connection and forces a full
            // re-handshake on next use; skip it when the token is unchanged.
            if client.http_headers_match(fresh_headers) {
                continue;
            }

            let headers = fresh_headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            *client = Arc::new(McpClient::new_http(
                client_name.clone(),
                HttpConfig {
                    url: fresh_endpoint.to_string(),
                    headers,
                },
                None,
                self.meta_config_map.get(client_name.as_str()),
            ));
            tracing::info!(server = %client_name, "Refreshed managed MCP client with fresh token");
        }
    }

    /// Look up a client by server name.
    /// Owned clients take priority (they can override inherited ones).
    pub fn get_client(&self, name: &str) -> Option<&Arc<McpClient>> {
        self.owned_clients
            .get(name)
            .or_else(|| self.shared_clients.get(name))
    }

    /// Iterate over all clients (owned first, then shared — skipping shared
    /// entries whose name is overridden by an owned client).
    pub fn all_clients(&self) -> impl Iterator<Item = (&McpServerName, &Arc<McpClient>)> {
        self.owned_clients.iter().chain(
            self.shared_clients
                .iter()
                .filter(|(name, _)| !self.owned_clients.contains_key(name.as_str())),
        )
    }

    /// Import shared clients from a parent pool snapshot.
    /// Clients whose name collides with an agent-definition-owned server
    /// are skipped (the owned server takes priority).
    pub fn import_shared_clients(&mut self, pool: &SharedMcpPool) {
        let config_names: std::collections::HashSet<&str> =
            self.configs.iter().map(mcp_server_name).collect();
        for (name, client) in &pool.clients {
            if !config_names.contains(name.as_str()) {
                self.shared_clients.insert(name.clone(), Arc::clone(client));
            }
        }
    }
}

/// Snapshot of an MCP connection pool, taken at subagent spawn time.
///
/// The HashMap is cloned (cheap — values are `Arc<McpClient>`), so the
/// subagent's map is independent of the parent's. The `Arc<McpClient>`
/// entries are shared — both parent and child use the same transport.
/// This is intentionally snapshot-based, not live-updating.
#[derive(Clone)]
pub struct SharedMcpPool {
    clients: HashMap<McpServerName, Arc<McpClient>>,
    configs: Vec<acp::McpServer>,
    meta_config_map: McpMetaConfigMap,
}

impl SharedMcpPool {
    /// Create a snapshot from an existing `McpState`.
    /// Captures both owned and shared clients (deduped — owned wins).
    pub fn from_state(state: &McpState) -> Self {
        Self {
            clients: state
                .all_clients()
                .map(|(k, v)| (k.clone(), Arc::clone(v)))
                .collect(),
            configs: state.configs.clone(),
            meta_config_map: state.meta_config_map.clone(),
        }
    }

    pub fn get_client(&self, name: &str) -> Option<&Arc<McpClient>> {
        self.clients.get(name)
    }

    pub fn len(&self) -> usize {
        self.clients.len()
    }

    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    pub fn server_names(&self) -> impl Iterator<Item = &str> {
        self.clients.keys().map(String::as_str)
    }

    pub fn configs(&self) -> &[acp::McpServer] {
        &self.configs
    }

    pub fn meta_config_map(&self) -> &McpMetaConfigMap {
        &self.meta_config_map
    }

    /// Retain only clients whose name satisfies `predicate`.
    ///
    /// Only filters the `clients` map. `configs` and `meta_config_map` are
    /// left unchanged — callers that need config-level consistency should
    /// filter those separately. In the subagent inheritance path this is
    /// fine because `import_shared_clients` only iterates `clients`.
    pub fn retain_clients(&mut self, predicate: impl Fn(&str) -> bool) {
        self.clients.retain(|name, _| predicate(name));
    }
}

/// Compare two MCP server config lists for equality.
///
/// Since `acp::McpServer` may not implement PartialEq, we serialize to JSON and compare.
/// This is only called during config updates, so the overhead is acceptable.
pub(crate) fn mcp_servers_equal(a: &[acp::McpServer], b: &[acp::McpServer]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    // Compare JSON serializations
    match (serde_json::to_string(a), serde_json::to_string(b)) {
        (Ok(a_json), Ok(b_json)) => a_json == b_json,
        _ => false, // If serialization fails, assume not equal
    }
}

/// Default timeout for an MCP server's `initialize` handshake & initial tool
/// listing, used when no override is supplied. 30s is generous enough that
/// cold-start `uvx` / `uv run --with` stdio servers that download deps on
/// first launch aren't killed mid-handshake. The shell resolves env / config /
/// requirements / remote overrides and injects them via `McpClientTimeoutOverrides`.
const DEFAULT_STARTUP_TIMEOUT_SECS: u64 = 30;

/// Default timeout for individual tool calls.
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 6000;

/// How long a stdio server gets to exit after its transport closes before
/// its process group is killed.
const STDIO_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// Timeout for OAuth metadata discovery when building an HTTP transport.
/// Bounds transport setup for servers without OAuth support.
const OAUTH_DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Per-MCP-server config overrides from `_meta.mcpConfig` in session/new or session/load.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerMetaConfig {
    /// Init handshake timeout in ms. Overrides config.toml `startup_timeout_sec`.
    #[serde(default)]
    pub startup_timeout_ms: Option<u64>,
    /// Per-tool-call timeout in ms. Overrides config.toml `tool_timeout_sec`.
    #[serde(default)]
    pub tool_timeout_ms: Option<u64>,
    /// Per-tool timeout overrides in ms: `{ "create_issue": 120000, "search": 30000 }`.
    /// Overrides config.toml `tool_timeouts` (and `tool_timeout_sec`) for matching tools.
    #[serde(default)]
    pub tool_timeouts_ms: Option<HashMap<ToolName, u64>>,
    /// Also keep the raw base64 in tool-result text (in addition to the
    /// vision-token rendering) so the agent can decode + forward it via
    /// path-based tools like `send_file`. Costs ~2× tokens per image.
    /// Default `false`. See [`format_mcp_image`].
    #[serde(default)]
    pub expose_image_base64: Option<bool>,
}

/// MCP server name → per-server config overrides from `_meta.mcpConfig`.
pub type McpMetaConfigMap = HashMap<McpServerName, McpServerMetaConfig>;

/// Parse `mcpConfig` from a session request's `_meta`. Empty map if absent/invalid.
pub fn parse_mcp_meta_config(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> McpMetaConfigMap {
    meta.and_then(|m| m.get("mcpConfig"))
        .and_then(|v| serde_json::from_value::<McpMetaConfigMap>(v.clone()).ok())
        .unwrap_or_default()
}

/// MCP initialization strategy. Defined in `xai-grok-telemetry`; re-exported
/// here so existing call sites continue to work.
pub use xai_grok_telemetry::enums::McpInitStrategy;

/// Parse a non-empty `server__tool` ID with one overlap-aware delimiter and
/// valid [`xai_tool_protocol::ToolId`] syntax.
pub fn parse_mcp_qualified_name(name: &str) -> Option<(xai_tool_protocol::ToolId, &str, &str)> {
    let delimiter = MCP_TOOL_NAME_DELIMITER.as_bytes();
    // Byte windows preserve both overlapping `__` boundaries in `___`.
    let mut boundaries = name
        .as_bytes()
        .windows(delimiter.len())
        .enumerate()
        .filter_map(|(index, window)| (window == delimiter).then_some(index));
    let boundary = boundaries.next()?;
    if boundaries.next().is_some() {
        return None;
    }
    let (server, tool_with_delimiter) = name.split_at(boundary);
    let tool = &tool_with_delimiter[MCP_TOOL_NAME_DELIMITER.len()..];
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((xai_tool_protocol::ToolId::new(name).ok()?, server, tool))
}

/// Parse an MCP tool name in `server__tool` format into owned segments.
pub fn parse_mcp_tool_name(name: &str) -> Option<(String, String)> {
    parse_mcp_qualified_name(name).map(|(_, server, tool)| (server.to_owned(), tool.to_owned()))
}

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("MCP client error: {0}")]
    ClientError(String),

    #[error("MCP server '{server}' timed out after {timeout_secs}s")]
    Timeout { server: String, timeout_secs: u64 },

    #[error("Failed to spawn MCP server '{server}': {source}")]
    SpawnFailed {
        server: String,
        source: std::io::Error,
    },

    #[error("MCP server '{server}' handshake failed: {source}")]
    HandshakeFailed {
        server: String,
        source: Box<ClientInitializeError>,
    },

    /// Pre-spawn gate: server needs OAuth but this session cannot complete interactive auth.
    #[error(
        "MCP server '{server}': Auth required (non-interactive session; authenticate in TUI or set Authorization header)"
    )]
    AuthRequired { server: String },

    #[error("MCP service error: {0}")]
    ServiceError(#[from] ServiceError),
}

impl McpError {
    fn timeout(server: &str, duration: std::time::Duration) -> Self {
        Self::Timeout {
            server: server.to_string(),
            timeout_secs: duration.as_secs(),
        }
    }

    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Timeout { .. })
    }

    pub fn error_category(&self) -> xai_file_utils::events::McpErrorCategory {
        use xai_file_utils::events::McpErrorCategory;
        match self {
            Self::SpawnFailed { .. } => McpErrorCategory::SpawnFailed,
            Self::Timeout { .. } => McpErrorCategory::Timeout,
            Self::HandshakeFailed { .. } => McpErrorCategory::HandshakeFailed,
            Self::AuthRequired { .. } => McpErrorCategory::AuthRequired,
            Self::ClientError(_) | Self::ServiceError(_) => McpErrorCategory::ClientError,
        }
    }

    pub fn server_name(&self) -> Option<&str> {
        match self {
            Self::SpawnFailed { server, .. }
            | Self::Timeout { server, .. }
            | Self::HandshakeFailed { server, .. }
            | Self::AuthRequired { server } => Some(server),
            Self::ClientError(_) | Self::ServiceError(_) => None,
        }
    }

    /// True if this error indicates the server rejected us for auth reasons (a
    /// credential re-fetch could help). Timeout/spawn failures can't be cured by
    /// re-fetching credentials, so they're never auth.
    pub fn is_auth_rejection(&self) -> bool {
        match self {
            Self::AuthRequired { .. } => true,
            Self::HandshakeFailed { source, .. } => is_auth_rejection_message(&source.to_string()),
            Self::ServiceError(e) => is_auth_rejection_message(&e.to_string()),
            Self::ClientError(s) => is_auth_rejection_message(s),
            Self::Timeout { .. } | Self::SpawnFailed { .. } => false,
        }
    }
}

/// True if an MCP error *message* indicates an auth rejection (vs. a transport
/// drop, timeout, or protocol error), so host recovery can decide whether a
/// credential re-fetch would help.
///
/// Matches auth wording and context-anchored 401 patterns only, so a bare digit
/// ("took 401ms", ports) can't trip it. Excludes 403/forbidden — a non-auth
/// policy denial here, not a credential problem.
pub fn is_auth_rejection_message(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    // Auth wording has no numeric component, so plain substrings are safe.
    if l.contains("auth required")
        || l.contains("authorizationrequired")
        || l.contains("authrequired")
        || l.contains("authentication")
        || l.contains("unauthorized")
    {
        return true;
    }
    // Require a non-alphanumeric (or end) after "401" so "http 401" matches but
    // "http 4012" (other status) and "http 401ms" (a duration) do not.
    [
        "status: 401",
        "status code 401",
        "http status 401",
        "http 401",
        "error 401",
    ]
    .iter()
    .any(|token| token_at_word_boundary(&l, token))
}

/// Whether `haystack` contains `token` at a right word boundary, so a
/// digit-terminated token (`...401`) doesn't match a longer run (`4012`) or an
/// adjacent unit (`401ms`).
///
/// Invariant: `token` must be ASCII (all callers pass ASCII literals). A
/// non-ASCII token could advance `from` mid-UTF-8 and panic on the next slice.
fn token_at_word_boundary(haystack: &str, token: &str) -> bool {
    debug_assert!(
        token.is_ascii(),
        "token_at_word_boundary requires an ASCII token"
    );
    let mut from = 0;
    while let Some(idx) = haystack[from..].find(token) {
        let end = from + idx + token.len();
        if !haystack[end..].starts_with(|c: char| c.is_ascii_alphanumeric()) {
            return true;
        }
        from += idx + 1;
    }
    false
}

#[derive(Clone)]
pub struct McpTool {
    name: String,
    description: String,
    server_name: String,
    mcp_state: Arc<Mutex<McpState>>,
    schema: serde_json::Value,
    meta: Option<serde_json::Value>,
}

/// Data needed to register an MCP tool via `register_erased()`.
///
/// MCP tools have two visibility audiences controlled by `_meta.ui.visibility`:
///
/// - **Model-visible** (default, or `["model", "app"]`): registered in `ToolBridge`
///   so the LLM can invoke them during a conversation.
/// - **App-visible only** (`["app"]`): not registered in `ToolBridge`, so the LLM
///   never sees them. These are UI-only actions (e.g. refresh buttons) surfaced to
///   the frontend via `x.ai/mcp/tools_changed` notifications and callable via
///   `x.ai/mcp/call`.
pub struct McpToolRegistration {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub tool: McpErasedTool,
    pub meta: Option<serde_json::Value>,
    pub model_visible: bool,
}

impl McpTool {
    /// Reconstruct an `McpTool` from its constituent parts. Used when stashing
    /// a disabled tool at runtime so it can be re-enabled without a full re-init.
    pub fn new(
        name: String,
        description: String,
        server_name: String,
        mcp_state: Arc<Mutex<McpState>>,
        schema: serde_json::Value,
        meta: Option<serde_json::Value>,
    ) -> Self {
        Self {
            name,
            description,
            server_name,
            mcp_state,
            schema,
            meta,
        }
    }

    /// Convert into the data needed for `ToolBridge::register_erased()`.
    ///
    /// Invalid or ambiguous qualified IDs and provider-invalid names are logged
    /// and skipped; the upstream connector must provide non-empty `server` and
    /// `tool` segments separated by exactly one `__` boundary.
    pub fn into_registration(self) -> Option<McpToolRegistration> {
        let qualified_name = format!(
            "{}{}{}",
            self.server_name, MCP_TOOL_NAME_DELIMITER, self.name
        );

        if parse_mcp_qualified_name(&qualified_name).is_none() {
            tracing::error!(
                server = %self.server_name,
                tool = %self.name,
                qualified = %qualified_name,
                "Skipping MCP tool with invalid or ambiguous qualified name"
            );
            return None;
        }
        if let Err(reason) = validate_tool_name(&qualified_name) {
            tracing::error!(
                tool_name = %qualified_name,
                server = %self.server_name,
                reason = %reason,
                "Skipping MCP tool with invalid name"
            );
            return None;
        }

        let description = self.description.clone();
        let input_schema = self.schema.clone();
        let meta = self.meta.clone();

        let model_visible = meta
            .as_ref()
            .and_then(|m| m.get("ui"))
            .and_then(|ui| ui.get("visibility"))
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().any(|s| s.as_str() == Some("model")))
            .unwrap_or(true); // default: visible to model

        Some(McpToolRegistration {
            name: qualified_name,
            description,
            input_schema,
            tool: McpErasedTool { tool: self },
            meta,
            model_visible,
        })
    }
}

/// MCP tool wrapper for runtime dispatch.
///
/// MCP tools are already untyped (JSON → JSON), so they implement
/// `xai_tool_runtime::Tool` directly instead of going through typed wrappers.
pub struct McpErasedTool {
    tool: McpTool,
}

impl std::fmt::Debug for McpErasedTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpErasedTool")
            .field("name", &self.tool.name)
            .field("server", &self.tool.server_name)
            .finish()
    }
}

impl ToolMetadata for McpErasedTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::MCP
    }

    fn description_template(&self) -> &str {
        &self.tool.description
    }
}

impl xai_tool_runtime::Tool for McpErasedTool {
    type Args = serde_json::Value;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        // Use the qualified name (server__tool) so that two MCP servers
        // exposing the same raw tool name get distinct LocalRegistry entries.
        let qualified = format!(
            "{}{}{}",
            self.tool.server_name, MCP_TOOL_NAME_DELIMITER, self.tool.name
        );
        xai_tool_protocol::ToolId::new(&qualified)
            .unwrap_or_else(|_| xai_tool_protocol::ToolId::new("mcp_tool").expect("valid"))
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(&self.tool.name, &self.tool.description)
    }

    async fn run(
        &self,
        _ctx: xai_tool_runtime::ToolCallContext,
        raw: serde_json::Value,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        let mcp_call_start = std::time::Instant::now();
        let (client, event_writer) = {
            let state = self.tool.mcp_state.lock().await;
            let c = Arc::clone(state.get_client(&self.tool.server_name).ok_or_else(|| {
                xai_tool_runtime::ToolError::custom(
                    "process_manager",
                    format!("MCP server '{}' not found", self.tool.server_name),
                )
            })?);
            (c, state.event_writer().clone())
        };

        let server = &self.tool.server_name;
        let tool = &self.tool.name;
        let tool_timeout = client.tool_timeout_for(tool);
        let qualified_name = format!("{}{}{}", server, MCP_TOOL_NAME_DELIMITER, tool);
        event_writer.emit(xai_file_utils::events::Event::McpToolCallStarted {
            server_name: server.clone(),
            tool_name: tool.clone(),
            call_id: qualified_name.clone(),
            timeout_sec: tool_timeout,
        });

        let mut auth_retry_attempted = false;
        let mut reconnect_attempted = false;
        let mut is_timeout = false;
        let ew = &event_writer;
        let dispatch_result = match self
            .try_call_tool(&client, &raw, &mut reconnect_attempted, &mut is_timeout, ew)
            .await
        {
            Ok(result) => Ok(result),
            Err(first_err) if client.has_auth() => {
                auth_retry_attempted = true;
                let reauth_ok = client.force_reauth(false).await;
                ew.emit(xai_file_utils::events::Event::McpAuthRetry {
                    server_name: server.clone(),
                    trigger: "tool_call_failed".to_string(),
                    success: reauth_ok,
                });
                if reauth_ok {
                    self.try_call_tool(&client, &raw, &mut reconnect_attempted, &mut is_timeout, ew)
                        .await
                        .map_err(|e| {
                            xai_tool_runtime::ToolError::custom("process_manager", e.to_string())
                        })
                } else {
                    Err(first_err)
                }
            }
            Err(e) => Err(e),
        };

        let call_result = match dispatch_result {
            Ok(result) => result,
            Err(e) => {
                ew.emit(xai_file_utils::events::Event::McpToolCallCompleted {
                    server_name: server.clone(),
                    tool_name: tool.clone(),
                    call_id: qualified_name,
                    duration_ms: mcp_call_start.elapsed().as_millis() as u64,
                    success: false,
                    is_timeout,
                    error: Some(e.to_string()),
                    reconnect_attempted,
                    auth_retry_attempted,
                });
                return Err(e);
            }
        };

        let is_error = call_result.is_error.unwrap_or(false);
        let mut output = if is_error {
            let error_msg = call_result
                .content
                .iter()
                .filter_map(|c| match c {
                    rmcp::model::ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            ToolOutput::MCP(MCPOutput::errored(tool.clone(), server.clone(), error_msg))
        } else {
            let expose_base64 = client.expose_image_base64();
            let parts: Vec<String> = call_result
                .content
                .into_iter()
                .filter_map(|c| match c {
                    rmcp::model::ContentBlock::Text(t) => Some(t.text),
                    rmcp::model::ContentBlock::Image(img) => {
                        Some(format_mcp_image(&img.mime_type, &img.data, expose_base64))
                    }
                    rmcp::model::ContentBlock::Resource(r) => match &r.resource {
                        rmcp::model::ResourceContents::BlobResourceContents {
                            mime_type,
                            blob,
                            ..
                        }
if mime_type
                            .as_deref()
                            .is_some_and(|m| m.starts_with("image/")) =>
                        {
                            let mime = mime_type.as_deref().unwrap();
                            Some(format_mcp_image(mime, blob, expose_base64))
                        }
                        _ => serde_json::to_string(&r).ok(),
                    },
                    _ => None,
                })
                .collect();
            let text = parts.join("\n");
            ToolOutput::MCP(MCPOutput::okay_output(tool.clone(), server.clone(), text))
        };

        if let ToolOutput::MCP(ref mut mcp_out) = output {
            mcp_out.auth_retry_attempted = auth_retry_attempted;
            mcp_out.reconnect_attempted = reconnect_attempted;
            mcp_out.is_timeout = is_timeout;
        }

        let success = !is_error;
        let duration_ms = mcp_call_start.elapsed().as_millis() as u64;
        let error_text = if is_error {
            match &output {
                ToolOutput::MCP(mcp) => match mcp.output() {
                    MCPOutputDetails::Error(e) => Some(e.clone()),
                    _ => None,
                },
                _ => None,
            }
        } else {
            None
        };
        event_writer.emit(xai_file_utils::events::Event::McpToolCallCompleted {
            server_name: server.clone(),
            tool_name: tool.clone(),
            call_id: qualified_name.clone(),
            duration_ms,
            success,
            is_timeout,
            error: error_text,
            reconnect_attempted,
            auth_retry_attempted,
        });
        xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::McpToolCalled {
            server_name: server.clone(),
            tool_name: tool.clone(),
            qualified_name,
            success,
            duration_ms,
        });
        Ok(output)
    }
}

/// Render an MCP image content block. The data URI is consumed by the
/// session-layer `extract_base64_images` and rendered as vision tokens.
/// When `expose_base64`, also emit a `<mcp_image_base64>` wrapper that
/// survives extraction (wrapper has no `data:image/` prefix → regex skips
/// it), exposing the raw bytes to the agent for path-based forwarding.
fn format_mcp_image(mime: &str, base64_data: &str, expose_base64: bool) -> String {
    if expose_base64 {
        format!(
            "data:{mime};base64,{base64_data}\n\
             <mcp_image_base64 mime=\"{mime}\">\n\
             {base64_data}\n\
             </mcp_image_base64>"
        )
    } else {
        format!("data:{mime};base64,{base64_data}")
    }
}

/// Check whether a `ServiceError` indicates the underlying transport has died
/// and a fresh connection could recover it.
fn is_retriable_transport_error(err: &ServiceError) -> bool {
    matches!(
        err,
        ServiceError::TransportClosed | ServiceError::TransportSend(_)
    )
}

/// Recover for every JSON-RPC code except the deterministic client set
/// {-32700, -32600, -32601, -32602} (those mean the request was wrong, not the session).
fn should_recover_mcp_error(code: i32) -> bool {
    use rmcp::model::ErrorCode;
    let deterministic_client_error = code == ErrorCode::PARSE_ERROR.0
        || code == ErrorCode::INVALID_REQUEST.0
        || code == ErrorCode::METHOD_NOT_FOUND.0
        || code == ErrorCode::INVALID_PARAMS.0;
    !deterministic_client_error
}

/// Recovers transport errors, and an HTTP `McpError` once per dispatch except
/// deterministic client codes and auth-class errors — a rebuild reuses stale
/// creds, so auth is routed to the re-auth paths instead.
fn should_recover_service_error(
    err: &ServiceError,
    is_http: bool,
    reconnect_attempted: bool,
) -> bool {
    is_retriable_transport_error(err)
        || matches!(
            err,
            ServiceError::McpError(e)
                if is_http
                    && !reconnect_attempted
                    && should_recover_mcp_error(e.code.0)
                    && !is_auth_rejection_message(e.message.as_ref())
        )
}

impl McpErasedTool {
    async fn try_call_tool(
        &self,
        client: &Arc<McpClient>,
        raw: &serde_json::Value,
        reconnect_attempted: &mut bool,
        is_timeout: &mut bool,
        ew: &xai_file_utils::events::EventWriter,
    ) -> Result<rmcp::model::CallToolResult, xai_tool_runtime::ToolError> {
        let mcp_service = client
            .ensure_initialized()
            .await
            .map_err(|e| xai_tool_runtime::ToolError::custom("process_manager", e.to_string()))?;
        let tool_timeout = client.tool_timeout_for(&self.tool.name);
        let timeout_duration = std::time::Duration::from_secs(tool_timeout);
        let mut params = CallToolRequestParams::new(self.tool.name.clone());
        params.arguments = raw.as_object().cloned();

        let result =
            tokio::time::timeout(timeout_duration, mcp_service.call_tool(params.clone())).await;

        match result {
            Ok(Ok(call_result)) => Ok(call_result),
            Ok(Err(service_err))
                if should_recover_service_error(
                    &service_err,
                    client.is_http(),
                    *reconnect_attempted,
                ) =>
            {
                self.recover_and_retry(
                    client,
                    params,
                    timeout_duration,
                    tool_timeout,
                    service_err,
                    reconnect_attempted,
                    is_timeout,
                    ew,
                )
                .await
            }
            Ok(Err(e)) => Err(xai_tool_runtime::ToolError::custom(
                "process_manager",
                e.to_string(),
            )),
            Err(_) => {
                *is_timeout = true;
                // Reset for the next call but don't retry — a slow side-effecting tool must not run twice.
                if client.is_http() && !*reconnect_attempted {
                    client.reset_transport().await;
                    *reconnect_attempted = true;
                }
                Err(xai_tool_runtime::ToolError::custom(
                    "process_manager",
                    format!(
                        "MCP tool '{}' timed out after {} seconds",
                        self.tool.name, tool_timeout
                    ),
                ))
            }
        }
    }

    /// On `recover()` failure surface the original error, else the retry error
    /// (preserves the auth signal managed re-auth reads from the string).
    #[allow(clippy::too_many_arguments)]
    async fn recover_and_retry(
        &self,
        client: &Arc<McpClient>,
        params: CallToolRequestParams,
        timeout_duration: std::time::Duration,
        tool_timeout: u64,
        original_err: ServiceError,
        reconnect_attempted: &mut bool,
        is_timeout: &mut bool,
        ew: &xai_file_utils::events::EventWriter,
    ) -> Result<rmcp::model::CallToolResult, xai_tool_runtime::ToolError> {
        *reconnect_attempted = true;
        tracing::warn!(
            server = self.tool.server_name.as_str(),
            tool = self.tool.name.as_str(),
            error = %original_err,
            "MCP transport error, attempting reconnect"
        );
        ew.emit(xai_file_utils::events::Event::McpTransportError {
            server_name: self.tool.server_name.clone(),
            tool_name: self.tool.name.clone(),
            error: original_err.to_string(),
        });
        let mcp_service = match client.recover().await {
            Ok(service) => {
                ew.emit(xai_file_utils::events::Event::McpTransportReconnect {
                    server_name: self.tool.server_name.clone(),
                    success: true,
                    error: None,
                });
                service
            }
            Err(e) => {
                ew.emit(xai_file_utils::events::Event::McpTransportReconnect {
                    server_name: self.tool.server_name.clone(),
                    success: false,
                    error: Some(e.to_string()),
                });
                return Err(xai_tool_runtime::ToolError::custom(
                    "process_manager",
                    original_err.to_string(),
                ));
            }
        };
        match tokio::time::timeout(timeout_duration, mcp_service.call_tool(params)).await {
            Ok(Ok(call_result)) => Ok(call_result),
            Ok(Err(retry_err)) => Err(xai_tool_runtime::ToolError::custom(
                "process_manager",
                retry_err.to_string(),
            )),
            Err(_) => {
                *is_timeout = true;
                Err(xai_tool_runtime::ToolError::custom(
                    "process_manager",
                    format!(
                        "MCP tool '{}' timed out after {} seconds",
                        self.tool.name, tool_timeout
                    ),
                ))
            }
        }
    }
}

/// Whether this session can complete an interactive (browser) OAuth flow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OauthInteractivity {
    Interactive,
    NonInteractive,
}

impl OauthInteractivity {
    /// Headless/SDK sessions set `non_interactive = true` and cannot complete browser OAuth.
    pub fn from_non_interactive(non_interactive: bool) -> Self {
        if non_interactive {
            Self::NonInteractive
        } else {
            Self::Interactive
        }
    }
}

/// Outcome of probing whether an HTTP/SSE MCP server needs OAuth and whether
/// we have credentials usable without an interactive browser flow.
enum HttpOauthPrep {
    /// Server does not advertise OAuth (or discovery failed conservatively).
    NoOauthSupport,
    /// Ready to connect with an auth manager (stored token works, or interactive deferred auth).
    ManagerReady(Arc<tokio::sync::Mutex<rmcp::transport::auth::AuthorizationManager>>),
    /// OAuth is required but cannot complete in non-interactive mode — do not start unauthenticated.
    NeedsInteractiveLogin,
}

impl HttpOauthPrep {
    /// Inconclusive OAuth probe (manager-create error, discovery error, or timeout):
    /// interactive proceeds as plain HTTP; non-interactive fails closed to avoid rmcp
    /// auth-worker stderr noise.
    fn on_probe_failure(mode: OauthInteractivity) -> Self {
        match mode {
            OauthInteractivity::Interactive => Self::NoOauthSupport,
            OauthInteractivity::NonInteractive => Self::NeedsInteractiveLogin,
        }
    }
}

/// Proactive OAuth discovery per RFC 8414 + 9728.
///
/// Creates an `AuthorizationManager` with our `CredentialStoreAdapter`,
/// discovers server metadata, and loads stored tokens if available.
///
/// With no stored tokens but server OAuth support, behavior splits on `mode`:
/// `Interactive` spawns the browser flow in the background (non-blocking; the
/// first tool call picks up tokens via `force_reauth` → `initialize_from_store`
/// once the user consents), while `NonInteractive` fails closed
/// (`NeedsInteractiveLogin`) rather than start an unauthenticated worker that
/// fatals with `Auth(AuthorizationRequired)` on stderr while the prompt still
/// succeeds.
///
/// Known gap: rmcp `get_access_token` returns stored tokens as-is when expiry
/// metadata is absent, so an expiry-less revoked token can still reach
/// `ManagerReady`.
async fn discover_and_prepare_auth(
    server_name: &str,
    server_url: &str,
    mode: OauthInteractivity,
) -> HttpOauthPrep {
    let Ok(parsed_url) = url::Url::parse(server_url) else {
        return HttpOauthPrep::NoOauthSupport;
    };
    let adapter =
        crate::credentials::McpCredentialStoreAdapter::new(server_name.to_string(), parsed_url);

    let mut manager = match rmcp::transport::auth::AuthorizationManager::new(server_url).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(server = server_name, %e, "Failed to create OAuth manager");
            // Non-interactive: fail closed — unauthenticated HTTP may still fatal in rmcp.
            return HttpOauthPrep::on_probe_failure(mode);
        }
    };
    manager.set_credential_store(adapter);

    if let Ok(true) = manager.initialize_from_store().await {
        // Stored creds may be expired/unrefreshable; in non-interactive mode that
        // still yields rmcp worker fatal AuthorizationRequired on stderr. This probe
        // shares the caller's 5s discovery timeout budget.
        if mode == OauthInteractivity::NonInteractive
            && let Err(e) = manager.get_access_token().await
        {
            tracing::warn!(
                server = server_name,
                error = %e,
                "Skipping OAuth MCP in non-interactive mode (stored credentials unusable); re-authenticate in TUI"
            );
            return HttpOauthPrep::NeedsInteractiveLogin;
        }
        tracing::info!(server = server_name, "Loaded stored OAuth credentials");
        return HttpOauthPrep::ManagerReady(Arc::new(tokio::sync::Mutex::new(manager)));
    }

    match manager.discover_metadata().await {
        Ok(metadata) => {
            manager.set_metadata(metadata);
            if mode == OauthInteractivity::NonInteractive {
                tracing::warn!(
                    server = server_name,
                    "Skipping OAuth MCP in non-interactive mode (no stored tokens); authenticate in TUI or set an Authorization header"
                );
                return HttpOauthPrep::NeedsInteractiveLogin;
            }
            tracing::info!(
                server = server_name,
                "Server supports OAuth but has no stored tokens"
            );
            HttpOauthPrep::ManagerReady(Arc::new(tokio::sync::Mutex::new(manager)))
        }
        Err(rmcp::transport::auth::AuthError::NoAuthorizationSupport) => {
            tracing::debug!(server = server_name, "Server does not support OAuth");
            HttpOauthPrep::NoOauthSupport
        }
        Err(e) => {
            tracing::warn!(server = server_name, %e, "OAuth discovery failed");
            HttpOauthPrep::on_probe_failure(mode)
        }
    }
}

/// Configuration for HTTP MCP server connection.
#[derive(Clone)]
pub struct HttpConfig {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

/// Newline-delimited JSON-RPC stdio transport whose read side survives a
/// single undecodable line.
///
/// Used instead of rmcp's `AsyncRwTransport` for two reasons:
/// - **Wire silence:** a bad line is skipped without replying, whereas rmcp
///   answers shape-mismatched JSON with a -32600 error — a reply an off-spec
///   server could echo back as more invalid input.
/// - **Telemetry:** each skip emits an `McpTransportDecodeError` event (with a
///   truncated sample of the offending line) so the failure is visible in the
///   session trace — rmcp's own tracing is not captured there.
///
/// We read lines ourselves (rather than via `FramedRead` + rmcp's codec) so
/// reading continues after a bad line; only a genuine end-of-stream returns
/// `None`. A stray non-JSON stdout line, a JSON-RPC batch array, or an
/// off-spec response therefore never collapses the transport ("Transport
/// closed" failing every in-flight request — the "connector shows but doesn't
/// work" report).
///
/// Generic over `R`/`W` so it can be unit-tested with in-memory pipes; the
/// production transport binds `ChildStdout`/`ChildStdin`.
struct ResilientRwTransport<R, W>
where
    R: AsyncRead,
    W: AsyncWrite,
{
    read: BufReader<R>,
    /// `Arc<Mutex<Option<…>>>` so `send` can return a `Send + 'static` future
    /// (the `Transport` contract) without borrowing `self`, and so `close` can
    /// drop the writer — mirrors rmcp's own `AsyncRwTransport`.
    write: Arc<Mutex<Option<W>>>,
    server_name: String,
    event_writer: xai_file_utils::events::EventWriter,
}

/// Max bytes of an offending line copied into the decode-error event.
const DECODE_ERROR_SAMPLE_LEN: usize = 200;

/// A line that failed to deserialize but is a JSON *notification* (an object
/// with a `method` and no `id`) is benign — many servers emit non-MCP / unknown
/// notifications (e.g. LSP-style). Skip those quietly instead of flagging a
/// decode error, mirroring rmcp's compatibility handling.
fn is_ignorable_notification(line: &[u8]) -> bool {
    match serde_json::from_slice::<serde_json::Value>(line) {
        Ok(v) => v.get("id").is_none() && v.get("method").and_then(|m| m.as_str()).is_some(),
        Err(_) => false,
    }
}

impl<R, W> ResilientRwTransport<R, W>
where
    R: AsyncRead + Send + Unpin,
    W: AsyncWrite + Send + Unpin + 'static,
{
    fn new(
        read: R,
        write: W,
        server_name: String,
        event_writer: xai_file_utils::events::EventWriter,
    ) -> Self {
        Self {
            read: BufReader::new(read),
            write: Arc::new(Mutex::new(Some(write))),
            server_name,
            event_writer,
        }
    }

    /// Record a skipped, undecodable stdout line: a `warn!` log plus an
    /// `McpTransportDecodeError` event carrying the serde error and a truncated
    /// sample of the raw line (the diagnostic the untagged-enum serde error
    /// alone lacks).
    fn record_decode_error(&self, line: &[u8], err: &serde_json::Error) {
        let sample: String = String::from_utf8_lossy(line)
            .chars()
            .take(DECODE_ERROR_SAMPLE_LEN)
            .collect();
        tracing::warn!(
            server = %self.server_name,
            error = %err,
            sample = %sample,
            "Skipping undecodable MCP stdout line; keeping transport alive",
        );
        self.event_writer
            .emit(xai_file_utils::events::Event::McpTransportDecodeError {
                server_name: self.server_name.clone(),
                error: err.to_string(),
                sample,
            });
    }
}

impl<R, W> Transport<RoleClient> for ResilientRwTransport<R, W>
where
    R: AsyncRead + Send + Unpin,
    W: AsyncWrite + Send + Unpin + 'static,
{
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let lock = self.write.clone();
        async move {
            let mut bytes = serde_json::to_vec(&item).map_err(std::io::Error::other)?;
            bytes.push(b'\n');
            let mut guard = lock.lock().await;
            match guard.as_mut() {
                Some(write) => {
                    write.write_all(&bytes).await?;
                    write.flush().await
                }
                None => Err(std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "transport is closed",
                )),
            }
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        loop {
            let mut line = Vec::new();
            match self.read.read_until(b'\n', &mut line).await {
                Ok(0) => return None, // genuine end-of-stream
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!(
                        server = %self.server_name,
                        error = %e,
                        "MCP stdio read error; closing transport",
                    );
                    return None;
                }
            }
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }

            match serde_json::from_slice::<RxJsonRpcMessage<RoleClient>>(&line) {
                Ok(msg) => return Some(msg),
                // The whole point: a single undecodable line must not
                // collapse the transport — skip it and keep reading.
                Err(err) => {
                    if is_ignorable_notification(&line) {
                        tracing::trace!(
                            server = %self.server_name,
                            "Ignoring unrecognized MCP notification",
                        );
                    } else {
                        self.record_decode_error(&line, &err);
                    }
                    continue;
                }
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        let mut guard = self.write.lock().await;
        drop(guard.take());
        Ok(())
    }
}

/// Stdio MCP transport with a non-panicking cleanup path.
///
/// Unlike `rmcp`'s `TokioChildProcess` (which `tokio::spawn`s from `Drop` and so
/// panics when dropped without an entered runtime), this wrapper's `Drop` is
/// best-effort: it reaps via the current runtime if present, else a short-lived
/// cleanup thread, so the child never leaks as a zombie. Since the caller's
/// `detach_command` `setsid`s the child into its own group, teardown also
/// `killpg`s the whole group via [`ProcessGroup`] to avoid orphaning
/// grandchildren (e.g. `npx` -> `node`) before reaping the leader.
pub struct SafeTokioChildProcess {
    child: Option<tokio::process::Child>,
    process_group: Option<ProcessGroup>,
    transport: ResilientRwTransport<tokio::process::ChildStdout, tokio::process::ChildStdin>,
}

impl SafeTokioChildProcess {
    /// `server_name` + `event_writer` are threaded into the transport so a
    /// skipped (undecodable) stdout line emits an `McpTransportDecodeError`
    /// event for that server.
    fn spawn(
        mut cmd: Command,
        server_name: String,
        event_writer: xai_file_utils::events::EventWriter,
    ) -> std::io::Result<(Self, Option<ChildStderr>)> {
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("stdin was already taken"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("stdout was already taken"))?;
        let stderr = child.stderr.take();

        // Best-effort: a missing group just degrades to direct-child-only cleanup.
        let process_group = match ProcessGroup::new() {
            Ok(mut group) => match group.attach(&child) {
                Ok(()) => Some(group),
                Err(e) => {
                    tracing::warn!("Failed to attach MCP child to process group: {e}");
                    None
                }
            },
            Err(e) => {
                tracing::warn!("Failed to create MCP child process group: {e}");
                None
            }
        };

        Ok((
            Self {
                child: Some(child),
                process_group,
                transport: ResilientRwTransport::new(stdout, stdin, server_name, event_writer),
            },
            stderr,
        ))
    }

    fn id(&self) -> Option<u32> {
        self.child.as_ref()?.id()
    }

    /// SIGKILLs the whole process group (child + grandchildren). Synchronous, so
    /// it's safe from `Drop`; the leader still needs reaping afterwards.
    fn kill_process_group(&self) {
        if let Some(group) = &self.process_group
            && let Err(e) = group.kill()
        {
            tracing::warn!("Error killing MCP child process group: {e}");
        }
    }

    async fn graceful_shutdown(&mut self) -> std::io::Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        self.transport.close().await?;

        let result = tokio::select! {
            _ = tokio::time::sleep(STDIO_SHUTDOWN_GRACE) => {
                self.kill_process_group();
                match child.kill().await {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        tracing::warn!("Error killing MCP child: {e}");
                        Err(e)
                    }
                }
            }
            res = child.wait() => {
                // Reap any grandchildren now, while the pgid is still kept alive
                // by them and before the reaped leader's pid can be reused.
                self.kill_process_group();
                match res {
                    Ok(status) => {
                        tracing::info!("MCP child exited gracefully {status}");
                        Ok(())
                    }
                    Err(e) => {
                        tracing::warn!("Error waiting for MCP child: {e}");
                        Err(e)
                    }
                }
            }
        };

        // Leader is reaped; drop the group so `Drop` can't `killpg` a now-reusable pid.
        self.process_group = None;
        result
    }
}

impl Drop for SafeTokioChildProcess {
    fn drop(&mut self) {
        // Group teardown is synchronous, so do it first regardless of runtime.
        self.kill_process_group();

        let Some(mut child) = self.child.take() else {
            return;
        };

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(e) = child.kill().await {
                    tracing::warn!("Error killing MCP child process: {e}");
                }
            });
        } else if let Err(e) = std::thread::Builder::new()
            .name("mcp-stdio-child-cleanup".to_string())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::warn!("Error creating runtime to clean up MCP child process: {e}");
                        if let Err(e) = child.start_kill() {
                            tracing::warn!("Error signaling MCP child process during drop: {e}");
                        }
                        return;
                    }
                };

                rt.block_on(async move {
                    if let Err(e) = child.kill().await {
                        tracing::warn!("Error killing MCP child process: {e}");
                    }
                });
            })
        {
            tracing::warn!("Error spawning MCP child cleanup thread: {e}");
        }
    }
}

impl Transport<RoleClient> for SafeTokioChildProcess {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.transport.send(item)
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        self.transport.receive()
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.graceful_shutdown().await
    }
}

/// Transport configuration before connection is established.
enum PendingTransport {
    Stdio(Box<SafeTokioChildProcess>),
    Http(HttpConfig),
    HttpAuth {
        config: HttpConfig,
        auth_manager: Arc<tokio::sync::Mutex<rmcp::transport::auth::AuthorizationManager>>,
    },
    /// In-process SDK MCP server reached over the ACP reverse channel
    /// (`x.ai/mcp/sdk_call`). Rebuildable from its `server_id` + invoker, so handshake
    /// failures restore like Http (unlike the consumed Stdio child).
    Acp {
        server_id: String,
        invoker: Arc<dyn crate::acp_transport::AcpReverseInvoker>,
    },
}

/// A connected MCP service (rmcp's RunningService wrapped in Arc).
/// Uses [`GrokClientHandler`] rather than rmcp's default `ClientInfo`
/// handler: rmcp 2.1 parameterizes `RunningService` over the handler
/// type, and `ClientInfo` is only a `ClientHandler` impl with no
/// notification routing. The custom handler keeps the same protocol
/// behavior (same `get_info`) while plumbing
/// `tools/list_changed` / `resources/list_changed` notifications
/// through to the session-actor dispatcher.
pub type McpService = Arc<RunningService<RoleClient, GrokClientHandler>>;

/// MCP client connection state machine.
///
/// Single-flight handshake invariant: at most one task at a time may run
/// the handshake. While the handshake is in flight the state is
/// [`ClientState::Initializing`]; the holder owns the transport for the
/// duration of [`McpClient::try_handshake`]. Concurrent callers of
/// [`McpClient::ensure_initialized`] observe [`ClientState::Initializing`]
/// and park on [`McpClient::init_done`] until the holder publishes a
/// result, instead of failing fast with
/// `"MCP client already initializing"` as in earlier versions.
enum ClientState {
    /// No transport configured. Reachable from:
    /// - [`McpClient::stub`] (test placeholder; `ensure_initialized`
    ///   returns a configuration error).
    /// - Stdio handshake failure (the spawned child process is consumed
    ///   by `client.serve` and cannot be reused — Http/HttpAuth keep
    ///   their `HttpConfig` clone and transition back to `Pending`).
    Empty,
    /// Transport is configured and ready for the next handshake.
    Pending(PendingTransport),
    /// A caller is currently inside [`McpClient::try_handshake`] and owns
    /// the transport. New callers MUST park on
    /// [`McpClient::init_done`] (with a bounded timeout) rather than
    /// attempt a parallel handshake. If the holder is cancelled or
    /// panics before publishing a result, an [`InitGuard`] restores the
    /// transport on a best-effort basis so other callers can retry.
    Initializing,
    /// Handshake completed; the service is reference-counted via `Arc`.
    Ready(McpService),
}

/// `Copy` projection of [`ClientState`] used for cheap state-machine
/// inspection (see [`McpClient::state_kind`]). Mirrors the variants
/// 1:1, dropping the payloads so callers can pattern-match without
/// borrowing the state mutex's inner data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientStateKind {
    Empty,
    Pending,
    Initializing,
    Ready,
}

/// Classification used by [`crate::liveness::spawn_transport_liveness`].
///
/// Returned by [`McpClient::liveness_check`] under a single state-mutex
/// acquisition, so the watcher's per-tick predicate is atomic.
///
/// `Transient` covers states the watcher should silently exit on
/// (re-handshake races, externally-reset transports, post-failure
/// `Empty` slots). Only `Ready + transport closed` produces a
/// `TransportClosed` ACP push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivenessCheck {
    /// `Ready` + `is_transport_closed() == false`. Keep polling.
    Healthy,
    /// `Ready` + `is_transport_closed() == true`. Emit + exit.
    TransportClosed,
    /// Anything else (`Initializing`, `Pending`, `Empty`). The
    /// watcher exits silently — the new state is being managed
    /// externally; if it returns to `Ready` the owner can re-arm.
    Transient,
}

/// Events emitted by a live MCP client to its session-side dispatcher.
///
/// Produced by three sources:
///
/// 1. [`crate::liveness::spawn_transport_liveness`] when an `is_healthy`
///    poll observes that the rmcp service loop has shut down its receiver
///    (`TransportClosed`).
/// 2. [`GrokClientHandler`] when the server pushes a notification we
///    care about — currently `notifications/tools/list_changed` and
///    `notifications/resources/list_changed`.
/// 3. The session/managed-config layer when a server is added, removed,
///    or successfully (re-)initialized.
///
/// Consumers fan these out to ACP `x.ai/mcp/server_status` after 50 ms
/// of tumbling-window coalescing keyed by `(server, kind)`; see the
/// session-actor `StatusDispatcher`.
#[derive(Debug, Clone)]
pub enum McpClientEvent {
    /// The rmcp service loop has terminated; the client is no longer
    /// usable for tool calls and must be torn down (or restarted).
    TransportClosed {
        server: McpServerName,
        /// Identity of the client whose transport closed (see
        /// [`McpClient::client_id`]). A mismatch with the client
        /// currently registered under `server` marks the event stale —
        /// it must not tear down the replacement. Every emitter holds the
        /// closing `McpClient`, so the id is always known.
        client_id: u64,
    },
    /// `ensure_initialized` returned `Err(_)`; `reason` is the full
    /// stringified error, surfaced verbatim to the client (no
    /// sanitization) so failures are easy to debug.
    HandshakeFailed {
        server: McpServerName,
        reason: String,
    },
    /// Server pushed `notifications/tools/list_changed`.
    ToolsChanged { server: McpServerName },
    /// Server pushed `notifications/resources/list_changed`.
    ResourcesChanged { server: McpServerName },
    /// Client transitioned to [`ClientState::Ready`]; dispatcher uses
    /// this to surface "ready" status without polling. Emitted from
    /// `ensure_initialized`; the dispatcher maps it to
    /// `reason=initialized` (NOT `reason=restart_succeeded`, which is
    /// reserved for the restart path).
    Ready { server: McpServerName },
    /// Managed/local config diff resolved. The dispatcher fans this
    /// out into one [`Self::ConfigAdded`] / [`Self::ConfigRemoved`]
    /// event per affected server before buffering.
    ConfigDiff {
        added: Vec<McpServerName>,
        removed: Vec<McpServerName>,
    },
    /// Per-server `(server, ConfigAdded)` fan-out variant produced by
    /// the dispatcher from a [`Self::ConfigDiff`]. Keeps the
    /// `kind ↔ event payload` invariant: storing a fake `Ready`
    /// payload at a `ConfigAdded` key would be a footgun whenever a
    /// real `Ready` and a `ConfigDiff` collided in the same coalesce
    /// window.
    ConfigAdded { server: McpServerName },
    /// Per-server `(server, ConfigRemoved)` fan-out variant — the
    /// dispatched analogue of [`Self::ConfigAdded`] for the removed
    /// set of a [`Self::ConfigDiff`].
    ConfigRemoved { server: McpServerName },
}

/// Discriminant for [`McpClientEvent`], used as the second half of the
/// coalescing key `(server, kind)`. Two events with the same
/// `(server, kind)` collapse into the latest one inside the
/// dispatcher's 50 ms window.
///
/// Distinct from [`McpClientEvent`] because the latter carries
/// payload (e.g. `reason` on `HandshakeFailed`) that we don't want
/// participating in equality / hashing.
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub enum McpClientEventKind {
    TransportClosed,
    HandshakeFailed,
    ToolsChanged,
    ResourcesChanged,
    Ready,
    ConfigAdded,
    ConfigRemoved,
}

impl McpClientEvent {
    /// Server name carried by the event, if any.
    ///
    /// Returns `None` only for [`McpClientEvent::ConfigDiff`] — that
    /// variant is fanned out per-server by the dispatcher into
    /// [`Self::ConfigAdded`] / [`Self::ConfigRemoved`], where each
    /// fan-out child has a single server name.
    pub fn server_name(&self) -> Option<&str> {
        match self {
            Self::TransportClosed { server, .. }
            | Self::HandshakeFailed { server, .. }
            | Self::ToolsChanged { server }
            | Self::ResourcesChanged { server }
            | Self::Ready { server }
            | Self::ConfigAdded { server }
            | Self::ConfigRemoved { server } => Some(server.as_str()),
            Self::ConfigDiff { .. } => None,
        }
    }
}

/// RAII guard that restores [`ClientState::Pending`] if the
/// [`McpClient::ensure_initialized`] holder is dropped before publishing
/// its handshake result (task cancellation, panic). Without this guard a
/// cancellation mid-handshake would leave `state` stuck in
/// [`ClientState::Initializing`] and every subsequent caller would block
/// until the wait-timeout fallback fires, then return an error — the
/// caller would have to call [`McpClient::reset_transport`]
/// manually to recover.
///
/// On the success path the holder calls [`Self::disarm`] before storing
/// `Ready`/`Empty` under the state lock, which converts the `Drop` into
/// a no-op. The guard never restores on the success path.
///
/// Drop uses [`tokio::sync::Mutex::try_lock`] because `Drop` runs
/// synchronously and we cannot block the runtime here. If the lock is
/// contended (extremely rare — the only competing locker is another
/// `ensure_initialized` caller which holds the lock for the duration of
/// a match arm, microseconds), the restore is skipped and the
/// inflight-wait timeout in `ensure_initialized` becomes the
/// last-resort recovery path.
struct InitGuard<'a> {
    state: &'a Mutex<ClientState>,
    init_done: &'a Notify,
    /// `Some` until [`Self::disarm`] is called. Holds the restorable
    /// transport (HTTP / HttpAuth) or `None` for Stdio (whose child
    /// process is consumed by `client.serve` and cannot be reused).
    restore: Option<PendingTransport>,
}

impl InitGuard<'_> {
    /// Mark the guard as having published a result. Subsequent `Drop`
    /// becomes a no-op so it doesn't fight with the holder's own
    /// state-store-under-the-lock or wake waiters twice.
    fn disarm(&mut self) {
        self.restore = None;
    }
}

impl Drop for InitGuard<'_> {
    fn drop(&mut self) {
        let Some(restore) = self.restore.take() else {
            return;
        };
        // Best-effort restore. `try_lock` cannot block the runtime from
        // inside Drop; on contention the slot stays Initializing and the
        // inflight-wait timeout becomes the recovery path.
        if let Ok(mut guard) = self.state.try_lock()
            && matches!(&*guard, ClientState::Initializing)
        {
            *guard = ClientState::Pending(restore);
        }
        // Notify whether or not we managed to restore — parked waiters
        // need to wake up and either retry against the restored
        // transport or hit the wait-timeout error path.
        self.init_done.notify_waiters();
    }
}

/// Build a restorable handle for a pending transport, or `None` if the
/// transport cannot be reused after a handshake failure.
///
/// `PendingTransport` deliberately does not implement `Clone`: the
/// `Stdio` variant owns a [`tokio::process::Child`] that is consumed by
/// `client.serve`, and a "restored" Stdio entry would be a dead handle.
/// HTTP and HttpAuth, by contrast, only carry config + an `Arc` to a
/// shared auth manager, so a clone is the canonical way to retry.
fn restorable_transport(pending: &PendingTransport) -> Option<PendingTransport> {
    match pending {
        PendingTransport::Http(cfg) => Some(PendingTransport::Http(cfg.clone())),
        PendingTransport::HttpAuth {
            config,
            auth_manager,
        } => Some(PendingTransport::HttpAuth {
            config: config.clone(),
            auth_manager: auth_manager.clone(),
        }),
        PendingTransport::Acp { server_id, invoker } => Some(PendingTransport::Acp {
            server_id: server_id.clone(),
            invoker: invoker.clone(),
        }),
        PendingTransport::Stdio(_) => None,
    }
}

/// Monotonic source for [`McpClient::client_id`]. Process-global so every
/// client instance — including test stubs — gets a unique identity.
static NEXT_CLIENT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_client_id() -> u64 {
    NEXT_CLIENT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub struct McpClient {
    /// Unique identity of this client *instance*. See [`Self::client_id`].
    client_id: u64,
    server_name: McpServerName,
    state: Mutex<ClientState>,
    /// Wakes [`Self::ensure_initialized`] callers that observed
    /// [`ClientState::Initializing`] and parked. Notified after each
    /// handshake attempt finishes (success **or** failure) and `state`
    /// has been updated. See [`ClientState`] for the single-flight
    /// invariant this preserves.
    ///
    /// Replaces the previous fail-fast
    /// `McpError::ClientError("MCP client already initializing")` branch
    /// which leaked into model-visible tool results whenever the model's
    /// first tool dispatch raced the session actor's background
    /// `get_tool_registrations` handshake.
    init_done: Notify,
    startup_timeout_sec: u64,
    tool_timeout_sec: u64,
    /// Per-tool timeout overrides in seconds. Looked up by tool name;
    /// falls back to `tool_timeout_sec` when a tool isn't listed.
    tool_timeouts: HashMap<ToolName, u64>,
    /// See [`McpServerMetaConfig::expose_image_base64`].
    expose_image_base64: bool,
    /// Shared `AuthorizationManager` for OAuth-enabled servers. `AuthClient`
    /// inside the transport holds a clone of this Arc so token updates are
    /// visible to both the transport and the re-auth path.
    auth_manager: Option<Arc<tokio::sync::Mutex<rmcp::transport::auth::AuthorizationManager>>>,
    /// Stored for OAuth clients so we can rebuild the transport after re-auth.
    http_config: Option<HttpConfig>,
    /// BYO OAuth config for the full browser flow fallback (when refresh fails).
    byo_oauth_config: Option<McpOAuthConfig>,
    /// Rate limit on this server's reconnect warnings; passed to each HTTP
    /// transport so rebuilds keep the limit.
    warn_budget: crate::mcp_http_client::WarnBudget,
    /// The transport to rebuild on a dead connection — see
    /// [`McpClient::reset_transport`]. `None` for transports that can't
    /// reconnect, e.g. Stdio (whose child process is consumed by the
    /// handshake and can't be restarted from here).
    reconnect: Option<PendingTransport>,
    /// Event sink for transport-closed pollers, server-pushed
    /// `tools/list_changed` / `resources/list_changed` notifications,
    /// and handshake failures.
    ///
    /// The slot is `Some` after [`Self::set_event_tx`] is called and
    /// `None` otherwise. The `Arc<Mutex<...>>` is **shared with
    /// [`GrokClientHandler`]** constructed by
    /// [`Self::make_client_handler`]: the handler holds a clone of
    /// the same Arc and reads through it on every notification.
    /// Snapshotting the slot at handshake time instead would mean any
    /// session that wired `notify_tx` post-handshake silently lost
    /// every `tools/list_changed` and `resources/list_changed` for the
    /// life of the connection.
    ///
    /// `None` in three cases:
    /// 1. Test stubs and standalone-pool fixtures that don't need
    ///    cross-component event flow.
    /// 2. Subagent / shared-pool snapshots — only the **parent** session
    ///    is the owner of these events. A subagent that inherits a
    ///    shared `Arc<McpClient>` reads tools through it but does not
    ///    install its own dispatcher; the parent's
    ///    [`crate::liveness::TransportLivenessHandle`] already covers it.
    /// 3. Brand-new clients before the session's per-server task has
    ///    called [`Self::set_event_tx`].
    ///
    /// `parking_lot::Mutex` is sufficient (and lighter than the previous
    /// `tokio::sync::Mutex`): the lock is never held across an
    /// `.await`, and the handler's `emit` path is short and
    /// allocation-free.
    notify_tx: SharedEventTx,
    /// RAII handle for the per-client transport-liveness poller.
    ///
    /// `Some` after [`Self::arm_liveness_watcher`] succeeds; `None`
    /// initially. The slot is also cleared by the poller itself when
    /// it exits (whether on `TransportClosed` or because the state
    /// machine drifted out of `Ready` during a re-handshake — see
    /// [`crate::liveness::spawn_transport_liveness`]) so subsequent
    /// `arm_liveness_watcher` calls aren't silently blocked by a
    /// dead-but-still-present handle.
    ///
    /// `parking_lot::Mutex` is sufficient because the lock is only ever
    /// held for the duration of a slot swap. The poller task uses an
    /// internal `Arc` clone of this mutex (the same memory) so it
    /// can clear the slot before `break`.
    liveness_handle: Arc<parking_lot::Mutex<Option<crate::liveness::TransportLivenessHandle>>>,
}

/// Shared sender slot type — the same Arc lives on the [`McpClient`]
/// and the [`GrokClientHandler`] it constructs during
/// [`McpClient::try_handshake`]. Mutating the slot via
/// [`McpClient::set_event_tx`] is observed by the live rmcp service
/// loop on the next notification, so there's no "snapshot at
/// handshake" hazard.
pub type SharedEventTx =
    Arc<parking_lot::Mutex<Option<tokio::sync::mpsc::UnboundedSender<McpClientEvent>>>>;

/// External-config overrides for an MCP server, surfaced to xai-grok-mcp
/// from whatever loader the host crate uses (e.g. the host's `config.toml` parser).
///
/// All fields are pre-precedence: [`McpClient::load_timeouts`] (and
/// [`McpClient::load_expose_image_base64`]) still apply the
/// `_meta > overrides > default` precedence on top. Owning these types here
/// keeps MCP transport state free of the host's TOML schema.
///
/// Name retained for call-site stability; struct now carries non-timeout
/// config too (e.g. [`Self::expose_image_base64`]).
#[derive(Default, Debug, Clone)]
pub struct McpClientTimeoutOverrides {
    /// Server startup timeout in seconds.
    pub startup_timeout_sec: Option<u64>,
    /// Default per-tool timeout in seconds (used when a tool has no entry in `tool_timeouts`).
    pub tool_timeout_sec: Option<u64>,
    /// Per-tool timeout overrides in seconds, keyed by tool name.
    pub tool_timeouts: Option<HashMap<String, u64>>,
    /// See [`McpServerMetaConfig::expose_image_base64`].
    pub expose_image_base64: Option<bool>,
}

impl McpClient {
    fn load_timeouts(
        overrides: Option<&McpClientTimeoutOverrides>,
        meta_config: Option<&McpServerMetaConfig>,
    ) -> (u64, u64, HashMap<ToolName, u64>) {
        // _meta > overrides > default; env / config / requirements / remote are
        // resolved by the shell and injected via `overrides.startup_timeout_sec`.
        let startup = meta_config
            .and_then(|mc| mc.startup_timeout_ms)
            .map(|ms| ms.div_ceil(1000))
            .or_else(|| overrides.and_then(|o| o.startup_timeout_sec))
            .unwrap_or(DEFAULT_STARTUP_TIMEOUT_SECS);

        let tool = meta_config
            .and_then(|mc| mc.tool_timeout_ms)
            .map(|ms| ms.div_ceil(1000))
            .or_else(|| overrides.and_then(|o| o.tool_timeout_sec))
            .unwrap_or(DEFAULT_TOOL_TIMEOUT_SECS);

        // Per-tool overrides: external base, _meta overrides on top.
        // Precedence: _meta per-tool > overrides per-tool > (falls back to server-level `tool`)
        let mut tool_timeouts = HashMap::new();

        // Layer 1: overrides (already in seconds)
        if let Some(o) = overrides
            && let Some(ref tt) = o.tool_timeouts
        {
            tool_timeouts.extend(tt.iter().map(|(k, v)| (k.clone(), *v)));
        }

        // Layer 2: _meta tool_timeouts_ms (milliseconds → seconds), overrides external config
        if let Some(mc) = meta_config
            && let Some(ref tt) = mc.tool_timeouts_ms
        {
            for (k, v) in tt {
                tool_timeouts.insert(k.clone(), v.div_ceil(1000));
            }
        }

        (startup, tool, tool_timeouts)
    }

    /// `_meta > overrides > default(false)`, mirroring [`Self::load_timeouts`].
    fn load_expose_image_base64(
        overrides: Option<&McpClientTimeoutOverrides>,
        meta_config: Option<&McpServerMetaConfig>,
    ) -> bool {
        meta_config
            .and_then(|mc| mc.expose_image_base64)
            .or_else(|| overrides.and_then(|o| o.expose_image_base64))
            .unwrap_or(false)
    }

    /// The ONLY place that writes the `McpClient { .. }` struct literal.
    /// Every constructor funnels through here so adding a field touches one
    /// site. `reconnect` is snapshotted from the transport before it is
    /// moved into [`ClientState::Pending`] (`None` for non-reconnectable
    /// transports like Stdio — see [`restorable_transport`]).
    #[allow(clippy::too_many_arguments)]
    fn new_with_transport(
        server_name: String,
        transport: PendingTransport,
        overrides: Option<&McpClientTimeoutOverrides>,
        meta_config: Option<&McpServerMetaConfig>,
        auth_manager: Option<Arc<tokio::sync::Mutex<rmcp::transport::auth::AuthorizationManager>>>,
        http_config: Option<HttpConfig>,
        byo_oauth_config: Option<McpOAuthConfig>,
    ) -> Self {
        let reconnect = restorable_transport(&transport);
        let (startup_timeout_sec, tool_timeout_sec, tool_timeouts) =
            Self::load_timeouts(overrides, meta_config);
        let expose_image_base64 = Self::load_expose_image_base64(overrides, meta_config);
        Self {
            client_id: next_client_id(),
            server_name,
            state: Mutex::new(ClientState::Pending(transport)),
            init_done: Notify::new(),
            startup_timeout_sec,
            tool_timeout_sec,
            tool_timeouts,
            expose_image_base64,
            auth_manager,
            http_config,
            byo_oauth_config,
            warn_budget: crate::mcp_http_client::WarnBudget::default(),
            reconnect,
            notify_tx: Arc::new(parking_lot::Mutex::new(None)),
            liveness_handle: Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    pub fn new_http_auth(
        server_name: String,
        config: HttpConfig,
        auth_manager: Arc<tokio::sync::Mutex<rmcp::transport::auth::AuthorizationManager>>,
        byo_oauth_config: Option<McpOAuthConfig>,
        overrides: Option<&McpClientTimeoutOverrides>,
        meta_config: Option<&McpServerMetaConfig>,
    ) -> Self {
        Self::new_with_transport(
            server_name,
            PendingTransport::HttpAuth {
                config: config.clone(),
                auth_manager: auth_manager.clone(),
            },
            overrides,
            meta_config,
            Some(auth_manager),
            Some(config),
            byo_oauth_config,
        )
    }

    pub fn has_auth(&self) -> bool {
        self.auth_manager.is_some()
    }

    /// Try to recover tokens from disk or via refresh — no browser flow.
    ///
    /// Returns true if valid tokens were found (from another session/process
    /// writing to the credential store, or a successful token refresh).
    /// Used by `retry_auth_required_servers` on overlay refresh.
    pub async fn try_reauth_from_disk(&self) -> bool {
        let (Some(auth_mgr), Some(config)) = (&self.auth_manager, &self.http_config) else {
            return false;
        };

        // Token-changed gate: rmcp's `initialize_from_store` returns Ok(true)
        // for any disk-resident creds regardless of expiry, so without
        // comparing against the in-memory token we'd claim "fresh tokens from
        // disk" on the same stale token that triggered this retry. The
        // downstream handshake would catch it, but the log line would lie
        // during incident debugging — and the divergence from `force_reauth`'s
        // gate is the exact invariant drift we just fixed there.
        {
            use oauth2::TokenResponse as _;
            let mut mgr = auth_mgr.lock().await;
            let token_before = mgr
                .get_credentials()
                .await
                .ok()
                .and_then(|(_, tok)| tok)
                .map(|t| t.access_token().secret().to_string());
            if let Ok(true) = mgr.initialize_from_store().await {
                let token_after = mgr
                    .get_credentials()
                    .await
                    .ok()
                    .and_then(|(_, tok)| tok)
                    .map(|t| t.access_token().secret().to_string());
                if token_after.is_some() && token_after != token_before {
                    tracing::info!(
                        server = self.server_name.as_str(),
                        "Loaded fresh tokens from disk"
                    );
                    drop(mgr);
                    self.replace_state(ClientState::Pending(PendingTransport::HttpAuth {
                        config: config.clone(),
                        auth_manager: auth_mgr.clone(),
                    }))
                    .await;
                    return true;
                }
            }
        }

        let refresh_ok = {
            let mgr = auth_mgr.lock().await;
            mgr.refresh_token().await.is_ok()
        };

        if refresh_ok {
            self.replace_state(ClientState::Pending(PendingTransport::HttpAuth {
                config: config.clone(),
                auth_manager: auth_mgr.clone(),
            }))
            .await;
            return true;
        }

        false
    }

    /// Force token acquisition and reset the transport so the next
    /// `ensure_initialized` rebuilds it with the fresh token.
    ///
    /// Tries in order:
    /// 1. Reload from disk (picks up tokens from background auth task)
    /// 2. Refresh via refresh_token grant
    /// 3. Full browser-based OAuth flow
    pub async fn force_reauth(&self, force: bool) -> bool {
        let (Some(auth_mgr), Some(config)) = (&self.auth_manager, &self.http_config) else {
            return false;
        };

        // Check if another process/session wrote *fresh* tokens to disk. We
        // must compare against the token we already had in memory — rmcp's
        // `initialize_from_store` returns Ok(true) for any disk-resident
        // credentials regardless of expiry, so without the token-changed
        // check we'd short-circuit on the same stale token that triggered
        // this re-auth in the first place (real bug: pressing the auth
        // shortcut on a server with an expired bearer + no refresh_token
        // would no-op and then 401 on the next handshake).
        //
        // Hold a single lock guard across `token_before` → `initialize_from_store`
        // → `token_after` so the comparison's invariant ("snapshot, reload,
        // re-read") can't be torn by an interleaved mutation.
        {
            use oauth2::TokenResponse as _;
            let mut mgr = auth_mgr.lock().await;
            let token_before = mgr
                .get_credentials()
                .await
                .ok()
                .and_then(|(_, tok)| tok)
                .map(|t| t.access_token().secret().to_string());
            if let Ok(true) = mgr.initialize_from_store().await {
                let token_after = mgr
                    .get_credentials()
                    .await
                    .ok()
                    .and_then(|(_, tok)| tok)
                    .map(|t| t.access_token().secret().to_string());
                if token_after.is_some() && token_after != token_before {
                    tracing::info!(
                        server = self.server_name.as_str(),
                        "Loaded fresh tokens from disk (background auth or other process)"
                    );
                    drop(mgr);
                    self.replace_state(ClientState::Pending(PendingTransport::HttpAuth {
                        config: config.clone(),
                        auth_manager: auth_mgr.clone(),
                    }))
                    .await;
                    return true;
                }
            }
        }

        // Try token refresh.
        let refresh_ok = {
            let mgr = auth_mgr.lock().await;
            mgr.refresh_token().await.is_ok()
        };

        if refresh_ok {
            tracing::info!(
                server = self.server_name.as_str(),
                "Token refreshed successfully (no browser)"
            );
            self.replace_state(ClientState::Pending(PendingTransport::HttpAuth {
                config: config.clone(),
                auth_manager: auth_mgr.clone(),
            }))
            .await;
            return true;
        }

        // Full browser-based OAuth flow.
        {
            tracing::info!(
                server = self.server_name.as_str(),
                "Falling back to browser auth"
            );
            if let Err(e) = crate::oauth::authenticate_mcp_server_dedup(
                &self.server_name,
                &config.url,
                auth_mgr,
                self.byo_oauth_config.as_ref(),
                force,
            )
            .await
            {
                tracing::warn!(
                    server = self.server_name.as_str(),
                    %e,
                    "Full re-authentication failed"
                );
                return false;
            }
        }

        self.replace_state(ClientState::Pending(PendingTransport::HttpAuth {
            config: config.clone(),
            auth_manager: auth_mgr.clone(),
        }))
        .await;
        true
    }

    /// Reset the transport so the next `ensure_initialized` rebuilds it with a
    /// fresh connection.
    ///
    /// Called when a tool call fails with a transport error (`TransportClosed`,
    /// `TransportSend`) — the underlying connection is dead but the server's
    /// addressing (URL/headers for HTTP, `server_id`/invoker for ACP) is still
    /// valid.
    ///
    /// Returns `true` if the transport was reset: HTTP/HttpAuth/ACP rebuild
    /// from the `reconnect` snapshot taken at construction. Returns `false`
    /// for clients whose `reconnect` is `None` (e.g. Stdio — dead child
    /// processes can't be restarted from here).
    async fn reset_transport(&self) -> bool {
        let Some(t) = self.reconnect.as_ref().and_then(restorable_transport) else {
            return false;
        };
        self.replace_state(ClientState::Pending(t)).await;
        tracing::info!(
            server = %self.server_name,
            "Reset transport for reconnect after transport failure"
        );
        true
    }

    /// `true` if this client has an HTTP/SSE transport. The explicit predicate
    /// for recovery gates (the proactive path only recovers HTTP clients).
    pub fn is_http(&self) -> bool {
        self.http_config.is_some()
    }

    /// `true` for an in-process SDK client reached over the ACP reverse channel
    /// (rather than HTTP/stdio). Gates liveness watching — see
    /// [`Self::arm_liveness_watcher`].
    pub fn is_acp(&self) -> bool {
        matches!(self.reconnect, Some(PendingTransport::Acp { .. }))
    }

    /// Read-only: do `headers` equal this client's current HTTP transport
    /// headers? Compares the full set order-insensitively (the caller's
    /// headers originate from a `HashMap`). Returns `false` for a client
    /// with no HTTP config.
    pub fn http_headers_match(&self, headers: &HashMap<String, String>) -> bool {
        let Some(config) = &self.http_config else {
            return false;
        };
        // Materialize into a map so a duplicate stored key collapses to one
        // entry, keeping the length comparison honest. HTTP header names are
        // case-insensitive, so normalize names to lowercase on both sides (the
        // crate already does this for `authorization`) and avoid a needless
        // rebuild on a pure casing difference. Values stay case-sensitive.
        let stored: HashMap<String, &str> = config
            .headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.as_str()))
            .collect();
        stored.len() == headers.len()
            && headers
                .iter()
                .all(|(k, v)| stored.get(&k.to_ascii_lowercase()) == Some(&v.as_str()))
    }

    /// Recover a dead transport in place: reset → re-handshake → re-arm the
    /// liveness watcher. Returns the live [`McpService`].
    ///
    /// The single recovery path for both the proactive HTTP recovery
    /// (`SessionActor::reset_http_client`, gated on [`Self::is_http`]) and the
    /// lazy `try_call_tool` retry. Rebuilds from the `reconnect` snapshot, so it
    /// covers HTTP/HttpAuth/ACP; `arm_liveness_watcher` self-gates for ACP.
    ///
    /// `Err` for a client with no restorable transport (e.g. Stdio — its child
    /// was consumed by the handshake).
    pub async fn recover(self: &Arc<Self>) -> Result<McpService, McpError> {
        // Coalesce concurrent recoveries: reset only when Ready; if already
        // non-Ready a recovery is in flight, so join its single-flight
        // ensure_initialized instead of racing a reset.
        if matches!(self.state_kind().await, ClientStateKind::Ready)
            && !self.reset_transport().await
        {
            return Err(McpError::ClientError(format!(
                "MCP client {} has no transport to recover",
                self.server_name,
            )));
        }
        let service = self.ensure_initialized().await?;
        // Re-arm liveness so the next close is detected again. A `false` return
        // with a wired sender is unexpected only for watched (non-ACP) clients.
        if !self
            .arm_liveness_watcher(crate::liveness::DEFAULT_POLL_INTERVAL)
            .await
            && !self.is_acp()
            && self.event_tx_clone().is_some()
        {
            tracing::warn!(
                server = %self.server_name,
                "recovery: liveness watcher not re-armed despite a wired event sender",
            );
        }
        Ok(service)
    }

    /// Replace [`Self::state`] under the lock and wake any
    /// [`Self::ensure_initialized`] callers parked on [`Self::init_done`]
    /// so they re-check the new state on their next loop iteration.
    ///
    /// Use this for *external* state transitions that need to invalidate
    /// in-flight waits (re-auth completions, transport resets) so a parked
    /// waiter doesn't sit on a stale [`ClientState::Initializing`] view of
    /// the world. `ensure_initialized` itself doesn't go through this
    /// helper because it already mints the new state under the lock it's
    /// holding and notifies waiters once at the end of the handshake
    /// attempt.
    async fn replace_state(&self, new_state: ClientState) {
        {
            let mut guard = self.state.lock().await;
            *guard = new_state;
        }
        self.init_done.notify_waiters();
    }

    pub fn new_stdio(
        server_name: String,
        transport: SafeTokioChildProcess,
        overrides: Option<&McpClientTimeoutOverrides>,
        meta_config: Option<&McpServerMetaConfig>,
    ) -> Self {
        Self::new_with_transport(
            server_name,
            PendingTransport::Stdio(Box::new(transport)),
            overrides,
            meta_config,
            None,
            None,
            None,
        )
    }

    /// Build a client for an in-process SDK MCP server reached over the ACP reverse
    /// channel. `server_id` is the id the agent echoes back in `x.ai/mcp/sdk_call`; the
    /// `invoker` performs the reverse request. Same downstream path as HTTP/stdio.
    pub fn new_acp(
        server_name: String,
        server_id: String,
        invoker: Arc<dyn crate::acp_transport::AcpReverseInvoker>,
        overrides: Option<&McpClientTimeoutOverrides>,
        meta_config: Option<&McpServerMetaConfig>,
    ) -> Self {
        Self::new_with_transport(
            server_name,
            PendingTransport::Acp { server_id, invoker },
            overrides,
            meta_config,
            None,
            None,
            None,
        )
    }

    pub fn new_http(
        server_name: String,
        config: HttpConfig,
        overrides: Option<&McpClientTimeoutOverrides>,
        meta_config: Option<&McpServerMetaConfig>,
    ) -> Self {
        Self::new_with_transport(
            server_name,
            PendingTransport::Http(config.clone()),
            overrides,
            meta_config,
            None,
            Some(config),
            None,
        )
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Unique identity of this client *instance*. Two clients for the
    /// same server name (e.g. a dead client and its replacement after
    /// a config remove+re-add) have different ids. Carried on
    /// [`McpClientEvent::TransportClosed`] so consumers can tell a
    /// death event for the current client from a stale predecessor's.
    pub fn client_id(&self) -> u64 {
        self.client_id
    }

    pub fn startup_timeout_sec(&self) -> u64 {
        self.startup_timeout_sec
    }

    pub fn tool_timeout_sec(&self) -> u64 {
        self.tool_timeout_sec
    }

    /// See [`McpServerMetaConfig::expose_image_base64`].
    pub fn expose_image_base64(&self) -> bool {
        self.expose_image_base64
    }

    /// Resolve the timeout for a specific tool.
    ///
    /// Precedence (highest → lowest):
    /// 1. `_meta.mcpConfig.<server>.toolTimeoutsMs.<tool>`
    /// 2. `config.toml [mcp_servers.<server>].tool_timeouts.<tool>`
    /// 3. `_meta.mcpConfig.<server>.toolTimeoutMs`
    /// 4. `config.toml [mcp_servers.<server>].tool_timeout_sec`
    /// 5. Default (60s)
    ///
    /// Steps 1–2 are already merged into `self.tool_timeouts` at construction;
    /// steps 3–5 are already resolved into `self.tool_timeout_sec`.
    pub fn tool_timeout_for(&self, tool_name: &str) -> u64 {
        self.tool_timeouts
            .get(tool_name)
            .copied()
            .unwrap_or(self.tool_timeout_sec)
    }

    /// Drive the MCP handshake to completion (or return the cached
    /// service if one is already established), with single-flight
    /// semantics that are safe under arbitrary concurrent callers.
    ///
    /// ## Concurrency contract
    ///
    /// At most one task at a time runs [`Self::try_handshake`]; that task
    /// holds the transport and observes [`ClientState::Initializing`].
    /// Other concurrent callers park on [`Self::init_done`] (with a
    /// bounded timeout) instead of issuing parallel handshakes or
    /// failing immediately. When the holder publishes a result, all
    /// parked waiters re-check `state` and either:
    ///
    /// - return the freshly-stored [`McpService`] (handshake succeeded),
    /// - take ownership of the freshly-restored transport and run their
    ///   own handshake (handshake failed but transport is restorable),
    /// - or surface the error (Stdio handshake failed → no restorable
    ///   transport → [`ClientState::Empty`]).
    ///
    /// This replaces the pre-fix behavior where concurrent callers got
    /// an immediate `McpError::ClientError("MCP client already
    /// initializing")` — surfaced inside model-visible tool results
    /// whenever the model's first tool call landed inside the session
    /// actor's background `get_tool_registrations` handshake, causing
    /// repeated retries that exhausted prompt budgets without ever
    /// reaching the actual MCP server.
    ///
    /// ## Cancellation safety
    ///
    /// If the holder is dropped (parent task cancelled, panic) before
    /// publishing a result, [`InitGuard`]'s `Drop` impl best-effort
    /// restores the transport (so future callers can retry without an
    /// explicit `reset_transport`) and wakes parked waiters. The
    /// restore uses [`tokio::sync::Mutex::try_lock`] because `Drop` is
    /// synchronous; on the rare contention case the slot stays
    /// `Initializing` and the wait-timeout fallback below surfaces a
    /// clear error rather than blocking forever.
    pub async fn ensure_initialized(&self) -> Result<McpService, McpError> {
        // Bound how long a parked caller waits on `init_done` before
        // surfacing an error. `try_handshake` is itself bounded by
        // `startup_timeout_sec`, so anything beyond that plus a 1 s margin
        // means the holder was dropped without restoring the transport
        // (cancellation under heavy contention) — wedging silently would
        // turn this into the exact "stuck client" failure mode the rest of
        // this rewrite is designed to eliminate.
        let inflight_wait =
            std::time::Duration::from_secs(self.startup_timeout_sec.saturating_add(1));

        // Drive the loop body until we either return directly or break
        // out with an owned `PendingTransport`. We deliberately use a
        // labelled `loop` with a `break <expr>` so the compiler proves
        // every arm of the inner match either diverges (return /
        // continue) or yields the transport — no `unreachable!()`
        // escape hatch needed.
        let pending: PendingTransport = loop {
            // Subscribe to `init_done` BEFORE inspecting `state` so a
            // wake-up fired between our state check and the wait can't be
            // lost. `tokio::sync::Notify` only delivers a permit to
            // notify-futures that exist at the time of `notify_waiters`.
            let notified = self.init_done.notified();
            tokio::pin!(notified);

            let mut guard = self.state.lock().await;
            // Swap the current state for `Initializing` up front and
            // match on the OWNED previous value. This avoids the
            // `match-by-ref → mem::replace → re-match → unreachable!()`
            // dance — the compiler can bind `ClientState::Pending(t)`
            // directly from an owned value with no irrefutable-let
            // hole. Non-Pending arms restore their original variant
            // before falling through; the lock is held the entire
            // window so the brief `Initializing` placeholder is
            // invisible to other callers. Cost is one trivial unit-
            // variant write per non-Pending call (plus an `Arc::clone`
            // on the Ready path) — negligible.
            match std::mem::replace(&mut *guard, ClientState::Initializing) {
                ClientState::Ready(service) => {
                    *guard = ClientState::Ready(service.clone());
                    return Ok(service);
                }
                ClientState::Empty => {
                    *guard = ClientState::Empty;
                    return Err(McpError::ClientError(format!(
                        "MCP client {} has no transport configured",
                        self.server_name,
                    )));
                }
                ClientState::Initializing => {
                    // Another caller already owns the slot. Restore the
                    // placeholder we just swapped in (semantically a
                    // no-op since `Initializing` is a unit variant),
                    // drop the lock, park on `init_done`.
                    *guard = ClientState::Initializing;
                    drop(guard);
                    match tokio::time::timeout(inflight_wait, notified.as_mut()).await {
                        Ok(()) => continue,
                        Err(_) => {
                            return Err(McpError::ClientError(format!(
                                "MCP client {} init still in progress after {}s",
                                self.server_name,
                                inflight_wait.as_secs(),
                            )));
                        }
                    }
                }
                // The single arm that KEEPS the `Initializing`
                // placeholder we swapped in — this caller becomes the
                // single-flight handshake holder for the duration of
                // `try_handshake` below.
                ClientState::Pending(transport) => break transport,
            }
        };

        // Lock released. Run the handshake outside the lock so other
        // callers can park on `init_done` instead of stalling on
        // `state.lock()`.

        // Clone the transport's restorable handle twice — once for
        // the failure-path retry below, once for the drop guard.
        // `PendingTransport` is intentionally not `Clone` (Stdio's
        // `TokioChildProcess` is unique), so the helper returns
        // `None` for Stdio (whose handshake failures cannot be
        // recovered without a fresh spawn).
        let restore = restorable_transport(&pending);
        let restore_for_guard = restorable_transport(&pending);

        // Drop guard: if `try_handshake` panics or is cancelled before
        // we publish a result, restore `Pending(restore)` so other
        // callers don't stall on `Initializing` forever. Disarm via
        // `disarm()` immediately before storing the real result.
        let mut init_guard = InitGuard {
            state: &self.state,
            init_done: &self.init_done,
            restore: restore_for_guard,
        };

        let handshake_start = std::time::Instant::now();
        let mut result = self.try_handshake(pending).await;

        let handshake_elapsed = handshake_start.elapsed().as_micros() as u64;
        tracing::info!(target: xai_grok_telemetry::instrumentation::TARGET, event = "timing", name = "mcp_try_handshake", elapsed_us = handshake_elapsed);
        // On handshake failure, if we have an auth_manager, try
        // refreshing the token and retrying once. Handles expired
        // access tokens loaded from disk — the handshake fails at the
        // transport layer before rmcp's transparent 401 refresh can
        // kick in. We attempt refresh on any failure (not just auth
        // errors) because the cost is low and error strings from
        // different MCP servers are not reliable to match.
        if result.is_err()
            && let (Some(auth_mgr), Some(config)) = (&self.auth_manager, &self.http_config)
        {
            tracing::info!(
                server = %self.server_name,
                "Handshake failed, attempting token refresh and retry"
            );
            let refresh_ok = {
                let mgr = auth_mgr.lock().await;
                mgr.refresh_token().await.is_ok()
            };
            if refresh_ok {
                let retry_transport = PendingTransport::HttpAuth {
                    config: config.clone(),
                    auth_manager: auth_mgr.clone(),
                };
                result = self.try_handshake(retry_transport).await;
            }
        }

        // Disarm before publishing the result so the drop guard
        // doesn't double-restore on the success path or fight with
        // the failure-path assignment below.
        init_guard.disarm();

        // Snapshot the event sender before we commit Ready/Pending/Empty
        // under the lock. We want to emit `HandshakeFailed` (on `Err`) or
        // signal the dispatcher to set status=ready (on `Ok`) AFTER
        // releasing the state lock, so a `state.lock().await` inside the
        // dispatcher (should one ever exist — none today) can't deadlock.
        //
        // The snapshot reads through the SHARED `Arc<Mutex<...>>`
        // slot. If the per-server task wired [`Self::set_event_tx`]
        // BEFORE invoking `get_tool_registrations` (the pattern in
        // `acp_session.rs`), this snapshot picks up the sender even
        // for the very first handshake.
        let event_tx = self.event_tx_clone();

        let outcome = {
            let mut guard = self.state.lock().await;
            match result {
                Ok(service) => {
                    let service = Arc::new(service);
                    *guard = ClientState::Ready(service.clone());
                    tracing::info!(
                        server = %self.server_name,
                        "MCP server initialized successfully"
                    );
                    Ok(service)
                }
                Err(e) => {
                    *guard = match restore {
                        Some(transport) => ClientState::Pending(transport),
                        None => ClientState::Empty,
                    };
                    tracing::warn!(
                        server = %self.server_name,
                        error = %e,
                        "MCP server init failed"
                    );
                    Err(e)
                }
            }
        };
        // Wake parked callers AFTER releasing the state lock so they
        // observe the freshly-stored Ready/Pending/Empty value.
        self.init_done.notify_waiters();

        // Notify the session-actor StatusDispatcher of the handshake
        // outcome, AFTER releasing the state lock. Best-effort: if the
        // receiver is gone (dispatcher torn down, subagent without
        // wiring) the send fails silently. The dispatcher is the only
        // path that turns these into ACP pushes — see the
        // `client_event_tx` field on `McpState`.
        if let Some(tx) = &event_tx {
            match &outcome {
                Ok(_) => {
                    let _ = tx.send(McpClientEvent::Ready {
                        server: self.server_name.clone(),
                    });
                }
                Err(e) => {
                    let _ = tx.send(McpClientEvent::HandshakeFailed {
                        server: self.server_name.clone(),
                        reason: e.to_string(),
                    });
                }
            }
        }
        outcome
    }

    /// Run the MCP handshake (no lock held).
    async fn try_handshake(
        &self,
        pending: PendingTransport,
    ) -> Result<rmcp::service::RunningService<RoleClient, GrokClientHandler>, McpError> {
        let timeout = std::time::Duration::from_secs(self.startup_timeout_sec);
        let name = &self.server_name;

        match pending {
            PendingTransport::Stdio(process) => {
                let handler = self.make_client_handler();
                tokio::time::timeout(timeout, handler.serve(*process))
                    .await
                    .map_err(|_| McpError::timeout(name, timeout))?
                    .map_err(|e| McpError::HandshakeFailed {
                        server: name.to_string(),
                        source: Box::new(e),
                    })
            }
            PendingTransport::Http(config) => {
                let transport =
                    Self::build_http_transport(&config, name, self.warn_budget.clone())?;
                let handler = self.make_client_handler();
                tokio::time::timeout(timeout, handler.serve(transport))
                    .await
                    .map_err(|_| McpError::timeout(name, timeout))?
                    .map_err(|e| McpError::HandshakeFailed {
                        server: name.to_string(),
                        source: Box::new(e),
                    })
            }
            PendingTransport::HttpAuth {
                config,
                auth_manager,
            } => {
                let mut headers = reqwest::header::HeaderMap::new();
                for (key, value) in &config.headers {
                    if key.eq_ignore_ascii_case("Authorization") {
                        continue;
                    }
                    if let (Ok(n), Ok(v)) = (
                        reqwest::header::HeaderName::from_bytes(key.as_bytes()),
                        value.parse::<reqwest::header::HeaderValue>(),
                    ) {
                        headers.insert(n, v);
                    }
                }
                ensure_figma_user_agent(&mut headers, name, &config.url);
                let http_client = reqwest::Client::builder()
                    .default_headers(headers)
                    .build()
                    .map_err(|e| {
                        McpError::ClientError(format!("Failed to build HTTP client: {e}"))
                    })?;
                // `AuthClient::new` wants an owned manager, but ours is shared
                // (`Arc`) with the OAuth flow; the struct is non_exhaustive, so
                // build with a throwaway manager and swap in the shared one.
                let placeholder_manager =
                    rmcp::transport::auth::AuthorizationManager::new(config.url.as_str())
                        .await
                        .map_err(|e| {
                            McpError::ClientError(format!("Failed to build OAuth client: {e}"))
                        })?;
                let mut auth_client =
                    rmcp::transport::auth::AuthClient::new(http_client, placeholder_manager);
                auth_client.auth_manager = auth_manager.clone();
                let mcp_http_client = crate::mcp_http_client::McpHttpClient::new(
                    auth_client,
                    name.as_str(),
                    self.warn_budget.clone(),
                );
                let transport_config =
                    StreamableHttpClientTransportConfig::with_uri(config.url.as_str());
                let transport =
                    StreamableHttpClientTransport::with_client(mcp_http_client, transport_config);
                let handler = self.make_client_handler();
                tokio::time::timeout(timeout, handler.serve(transport))
                    .await
                    .map_err(|_| McpError::timeout(name, timeout))?
                    .map_err(|e| McpError::HandshakeFailed {
                        server: name.to_string(),
                        source: Box::new(e),
                    })
            }
            PendingTransport::Acp { server_id, invoker } => {
                // Per-reverse-call backstop on `x.ai/mcp/sdk_call`: the larger of the
                // startup and tool timeouts, so it never undercuts the real outer bound
                // (the handshake `initialize` is bounded by the serve `timeout` below;
                // tool calls by `tool_timeout_for` in `try_call_tool`). The bridge
                // forwards raw JSON-RPC without the tool name, so per-TOOL overrides
                // aren't applied here in v1; the HTTP path still honors them.
                let invoke_timeout = std::time::Duration::from_secs(
                    self.startup_timeout_sec.max(self.tool_timeout_sec),
                );
                let transport =
                    crate::acp_transport::acp_bridge_transport(server_id, invoker, invoke_timeout);
                let handler = self.make_client_handler();
                tokio::time::timeout(timeout, handler.serve(transport))
                    .await
                    .map_err(|_| McpError::timeout(name, timeout))?
                    .map_err(|e| McpError::HandshakeFailed {
                        server: name.to_string(),
                        source: Box::new(e),
                    })
            }
        }
    }

    fn make_client_info(server_name: &str) -> ClientInfo {
        let mut extensions = rmcp::model::ExtensionCapabilities::new();
        extensions.insert(
            "io.modelcontextprotocol/ui".to_string(),
            serde_json::from_value(serde_json::json!({
                "mimeTypes": ["text/html;profile=mcp-app"]
            }))
            .unwrap_or_default(),
        );
        let mut capabilities = ClientCapabilities::default();
        capabilities.extensions = Some(extensions);
        ClientInfo::new(
            capabilities,
            Implementation::new(
                format!("grok-shell-{server_name}"),
                xai_grok_version::VERSION.to_string(),
            ),
        )
        // rmcp's default `ProtocolVersion` tracks its LATEST; pin explicitly
        // so the advertised protocol only changes deliberately, never as a
        // side effect of an rmcp bump.
        .with_protocol_version(rmcp::model::ProtocolVersion::V_2025_06_18)
    }

    /// Build the [`GrokClientHandler`] that drives `client.serve(...)`.
    ///
    /// The handler holds a **clone of `Arc<Mutex<Option<Sender>>>`**,
    /// not a snapshot — so any subsequent call to
    /// [`Self::set_event_tx`] is observed by the live rmcp service
    /// loop on its next notification.
    fn make_client_handler(&self) -> GrokClientHandler {
        GrokClientHandler {
            info: Self::make_client_info(&self.server_name),
            server_name: self.server_name.clone(),
            notify_tx: Arc::clone(&self.notify_tx),
        }
    }

    /// Wire a sender for [`McpClientEvent`]s emitted by this client.
    ///
    /// Mutates the shared slot synchronously. All previously-cloned
    /// references (the [`GrokClientHandler`] handed to
    /// `client.serve`, the [`crate::liveness::spawn_transport_liveness`]
    /// task) read through the same Arc, so this is observed
    /// session-wide on the next event.
    pub fn set_event_tx(&self, tx: Option<tokio::sync::mpsc::UnboundedSender<McpClientEvent>>) {
        *self.notify_tx.lock() = tx;
    }

    /// Snapshot the current event sender, if any.
    ///
    /// Used by [`crate::liveness::spawn_transport_liveness`] (which
    /// captures a `Sender` clone at spawn time) and by
    /// [`Self::ensure_initialized`]'s post-handshake emit. Synchronous
    /// because the shared slot is a `parking_lot::Mutex`.
    pub fn event_tx_clone(&self) -> Option<tokio::sync::mpsc::UnboundedSender<McpClientEvent>> {
        self.notify_tx.lock().clone()
    }

    /// Install or replace this client's transport-liveness handle.
    /// Dropping the previous handle (if any) cancels its task; the
    /// new handle starts polling on its own schedule. Pass `None` to
    /// stop watching without installing a new one.
    ///
    /// Synchronous: the slot is a `parking_lot::Mutex`. The poller
    /// task uses an `Arc` clone of this same mutex so it can clear the
    /// slot from inside the task before exiting.
    pub fn set_liveness_handle(&self, handle: Option<crate::liveness::TransportLivenessHandle>) {
        *self.liveness_handle.lock() = handle;
    }

    /// Arm the per-client transport-liveness watcher.
    ///
    /// Idempotent and gated:
    /// - Returns `false` for in-process SDK ([`Self::is_acp`]) clients: the
    ///   watcher's only output is `TransportClosed`, which the dispatcher can't
    ///   recover for ACP (not in `configs`), so it would evict the client. ACP
    ///   recovers lazily via [`Self::reset_transport`] instead. Gated here so no
    ///   caller can forget it.
    /// - Returns `false` if there's no `notify_tx` wired (subagent
    ///   snapshot or pre-dispatcher state) — nothing to do.
    /// - Returns `false` if the client isn't `Ready` — armed pollers
    ///   would just exit silently on their first poll, but skipping
    ///   the spawn entirely is cheaper.
    /// - Returns `false` if a live handle is already installed.
    /// - Otherwise spawns the poller and stores the handle.
    ///
    /// **TOCTOU note**: the state check is performed before the
    /// liveness lock is acquired. A concurrent re-handshake could move
    /// the state to `Initializing` between the check and the spawn.
    /// This is benign — the poller's first tick observes the
    /// non-`Ready` state and exits silently without emitting. So the
    /// worst case under TOCTOU is "the poller starts and immediately
    /// stops"; it never produces a spurious `TransportClosed`.
    ///
    /// Lifecycle: when the watcher emits `TransportClosed` it clears
    /// the slot itself; the next `arm_liveness_watcher` call can
    /// install a fresh handle without a manual
    /// [`Self::set_liveness_handle`] reset.
    pub async fn arm_liveness_watcher(
        self: &Arc<Self>,
        poll_interval: std::time::Duration,
    ) -> bool {
        if self.is_acp() {
            return false;
        }
        let Some(event_tx) = self.event_tx_clone() else {
            return false;
        };
        if !matches!(self.state_kind().await, ClientStateKind::Ready) {
            return false;
        }
        let mut slot = self.liveness_handle.lock();
        if slot.is_some() {
            return false;
        }
        let handle = crate::liveness::spawn_transport_liveness(
            self.server_name.clone(),
            Arc::clone(self),
            poll_interval,
            event_tx,
            Arc::clone(&self.liveness_handle),
        );
        *slot = Some(handle);
        true
    }

    fn build_http_transport(
        config: &HttpConfig,
        server_name: &str,
        warn_budget: crate::mcp_http_client::WarnBudget,
    ) -> Result<
        StreamableHttpClientTransport<crate::mcp_http_client::McpHttpClient<reqwest::Client>>,
        McpError,
    > {
        let mut headers = reqwest::header::HeaderMap::new();
        for (key, value) in &config.headers {
            match (
                reqwest::header::HeaderName::from_bytes(key.as_bytes()),
                value.parse::<reqwest::header::HeaderValue>(),
            ) {
                (Ok(name), Ok(val)) => {
                    headers.insert(name, val);
                }
                _ => {
                    tracing::warn!("Skipping invalid MCP HTTP header: {key}");
                }
            }
        }
        ensure_figma_user_agent(&mut headers, server_name, &config.url);
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .map_err(|e| McpError::ClientError(format!("Failed to build HTTP client: {e}")))?;
        let mcp_http_client =
            crate::mcp_http_client::McpHttpClient::new(client, server_name, warn_budget);
        let transport_config = StreamableHttpClientTransportConfig::with_uri(config.url.as_str());
        Ok(StreamableHttpClientTransport::with_client(
            mcp_http_client,
            transport_config,
        ))
    }

    /// Cheap, non-blocking liveness predicate.
    ///
    /// Inspects the current [`ClientState`] under the state mutex only —
    /// it MUST NOT call [`Self::ensure_initialized`] or any other path
    /// that can trigger a network round-trip. The previous implementation
    /// went through `ensure_initialized`, which could block UI callers
    /// (e.g. an MCP status modal) for up to `startup_timeout_sec` seconds
    /// on a dead stdio server.
    ///
    /// Semantics:
    /// - `Ready(service)` with an open transport → `true`.
    /// - `Ready(service)` whose receiver-side has been dropped (typically
    ///   because the rmcp service loop terminated) → `false`. rmcp 2.1
    ///   `Peer::is_transport_closed` reports `self.tx.is_closed()` at
    ///   `service.rs:703-705`; `RunningService` derefs to `Peer` at
    ///   `service.rs:716-722`.
    /// - Any other variant (`Empty`, `Pending`, `Initializing`) →
    ///   `false`.
    ///
    /// HTTP idle caveat: for [`StreamableHttpClientTransport`] the rmcp
    /// service loop only terminates on an outgoing send failure or an
    /// explicit shutdown. A long-idle HTTP server therefore keeps
    /// `is_transport_closed()` returning `false`, and this method
    /// continues to report `true`. That is the desired semantics — a
    /// liveness probe would belong in a separate watcher, not here.
    pub async fn is_healthy(&self) -> bool {
        let guard = self.state.lock().await;
        match &*guard {
            ClientState::Ready(service) => !service.is_transport_closed(),
            _ => false,
        }
    }

    /// Atomic classification for the liveness watcher.
    ///
    /// Reads `state` once and projects onto
    /// [`LivenessCheck`]: distinguishes "transport actually closed"
    /// (emit + exit) from "state moved out of `Ready`" (exit
    /// silently). The watcher depends on this distinction: a plain
    /// `is_healthy`-based predicate cannot tell the cases apart and
    /// would false-fire `TransportClosed` on re-handshake transitions.
    pub async fn liveness_check(&self) -> LivenessCheck {
        let guard = self.state.lock().await;
        match &*guard {
            ClientState::Ready(service) => {
                if service.is_transport_closed() {
                    LivenessCheck::TransportClosed
                } else {
                    LivenessCheck::Healthy
                }
            }
            _ => LivenessCheck::Transient,
        }
    }

    /// State-machine snapshot for diagnostics and downstream UI.
    ///
    /// Like [`Self::is_healthy`], this is a cheap state inspection
    /// (no handshake, no network I/O). Maps [`ClientState`] onto a
    /// `Copy` enum so callers can match without holding a reference to
    /// the inner [`McpService`] / [`PendingTransport`].
    pub async fn state_kind(&self) -> ClientStateKind {
        let guard = self.state.lock().await;
        match &*guard {
            ClientState::Empty => ClientStateKind::Empty,
            ClientState::Pending(_) => ClientStateKind::Pending,
            ClientState::Initializing => ClientStateKind::Initializing,
            ClientState::Ready(_) => ClientStateKind::Ready,
        }
    }

    /// Materialize this server's tool descriptors as JSON files under
    /// `<server_dir>/tools/`.
    ///
    /// The model reads these before issuing an MCP tool call.
    /// Each tool becomes `<server_dir>/tools/<sanitized_tool_name>.json`
    /// with `{name, description, inputSchema}`. Resources are intentionally
    /// not materialized: this harness exposes only MCP tool calls, so resource
    /// descriptors would advertise MCP-resource tools the model can't use.
    ///
    /// Best-effort: errors writing individual descriptors are logged but
    /// don't abort the materialization. Returns the number of files written.
    pub async fn materialize_descriptors(
        &self,
        server_dir: &std::path::Path,
    ) -> Result<usize, McpError> {
        let mcp_service = self.ensure_initialized().await?;

        // Collect descriptors via the async MCP API, then defer all filesystem
        // work to a single `spawn_blocking` so the executor is never blocked on
        // `std::fs` (this runs on every MCP tool-set change, not just startup).
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let result = mcp_service
                .list_tools(Some(
                    PaginatedRequestParams::default().with_cursor(cursor.clone()),
                ))
                .await?;
            for tool in result.tools {
                let descriptor = serde_json::json!({
                    "name": tool.name.as_ref(),
                    "description": tool.description.as_deref(),
                    "inputSchema": tool.input_schema.as_ref(),
                });
                match serde_json::to_vec_pretty(&descriptor) {
                    Ok(bytes) => files.push((
                        format!("{}.json", sanitize_descriptor_segment(tool.name.as_ref())),
                        bytes,
                    )),
                    Err(e) => tracing::warn!(
                        tool = %tool.name.as_ref(),
                        error = %e,
                        "failed to serialize MCP tool descriptor",
                    ),
                }
            }
            match result.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        // Write each descriptor atomically (temp file + rename) so a concurrent
        // reader never sees a half-written JSON and overlapping writers converge
        // without a lock.
        let tools_dir = server_dir.join("tools");
        tokio::task::spawn_blocking(move || -> Result<usize, McpError> {
            std::fs::create_dir_all(&tools_dir).map_err(|e| {
                McpError::ClientError(format!(
                    "failed to create MCP tools descriptor dir {}: {e}",
                    tools_dir.display()
                ))
            })?;
            let mut written = 0usize;
            for (file_name, bytes) in files {
                let path = tools_dir.join(&file_name);
                let write_result =
                    tempfile::NamedTempFile::new_in(&tools_dir).and_then(|mut tmp| {
                        std::io::Write::write_all(&mut tmp, &bytes)?;
                        tmp.persist(&path).map_err(|e| e.error)
                    });
                match write_result {
                    Ok(_) => written += 1,
                    Err(e) => tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "failed to write MCP tool descriptor",
                    ),
                }
            }
            Ok(written)
        })
        .await
        .map_err(|e| McpError::ClientError(format!("descriptor write task panicked: {e}")))?
    }

    /// Read the server's `instructions` from the MCP initialize handshake.
    /// Returns `None` if the client isn't ready yet.
    pub async fn server_instructions(&self) -> Option<String> {
        let guard = self.state.lock().await;
        if let ClientState::Ready(service) = &*guard {
            service
                .peer_info()?
                .instructions
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(String::from)
        } else {
            None
        }
    }

    pub async fn get_tool_registrations(
        &self,
        mcp_state: Arc<Mutex<McpState>>,
    ) -> Result<Vec<McpToolRegistration>, McpError> {
        let _ensure_init_timer =
            xai_grok_telemetry::instrumentation::timer("mcp_ensure_initialized");
        let mcp_service = self.ensure_initialized().await?;

        let mut all_tools = Vec::new();
        let mut cursor: Option<String> = None;

        let _list_tools_timer = xai_grok_telemetry::instrumentation::timer("mcp_list_tools");
        loop {
            let list_tools_result = mcp_service
                .list_tools(Some(
                    PaginatedRequestParams::default().with_cursor(cursor.clone()),
                ))
                .await?;

            all_tools.extend(list_tools_result.tools);

            match list_tools_result.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        let registrations: Vec<_> = all_tools
            .into_iter()
            .filter_map(|tool| {
                let meta: Option<serde_json::Value> = tool
                    .meta
                    .as_ref()
                    .and_then(|m| serde_json::to_value(m).ok());

                let name = tool.name.to_string();
                let description = tool.description.map(|d| d.to_string()).unwrap_or_default();
                let mut schema = serde_json::to_value(tool.input_schema.as_ref())
                    .unwrap_or_else(|_| serde_json::json!({"type": "object"}));
                // Ensure the schema has "type": "object" — some MCP servers
                // (e.g., VSCode) send `inputSchema: {}` for tools with no
                // parameters. Azure's OpenAI API rejects schemas without a
                // `type` field with: 'schema must be a JSON Schema of type:
                // "object", got type: "None"'.
                if let Some(obj) = schema.as_object_mut() {
                    obj.entry("type")
                        .or_insert_with(|| serde_json::json!("object"));
                    obj.entry("properties")
                        .or_insert_with(|| serde_json::json!({}));
                }

                let mcp_tool = McpTool {
                    name,
                    description,
                    server_name: self.server_name.clone(),
                    mcp_state: Arc::clone(&mcp_state),
                    schema,
                    meta,
                };
                // Invalid tools (bad names) return None and are skipped
                mcp_tool.into_registration()
            })
            .collect();

        // Warn about tool_timeouts keys that don't match any discovered tool.
        // This catches typos like `creat_issue` instead of `create_issue`.
        if !self.tool_timeouts.is_empty() {
            // Registration names are qualified ("server__tool"); tool_timeouts
            // keys are raw tool names. Strip the server prefix for comparison.
            let prefix = format!("{}{}", self.server_name, MCP_TOOL_NAME_DELIMITER);
            let raw_names: Vec<&str> = registrations
                .iter()
                .map(|r| r.name.strip_prefix(prefix.as_str()).unwrap_or(&r.name))
                .collect();
            let discovered: std::collections::HashSet<&str> = raw_names.iter().copied().collect();
            for key in self.tool_timeouts.keys() {
                if !discovered.contains(key.as_str()) {
                    tracing::info!(
                        server = %self.server_name,
                        tool_timeout_key = %key,
                        "tool_timeouts entry '{}' does not match any tool exposed by MCP server '{}' \
                         (available: {}). The per-tool timeout will have no effect — check for typos.",
                        key,
                        self.server_name,
                        raw_names.join(", "),
                    );
                }
            }
        }

        Ok(registrations)
    }

    /// Call a tool directly on the MCP server (for testing/debugging).
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<rmcp::model::CallToolResult, McpError> {
        let mcp_service = self.ensure_initialized().await?;
        let result = mcp_service
            .call_tool({
                let mut params = CallToolRequestParams::new(tool_name.to_string());
                params.arguments = arguments.as_object().cloned();
                params
            })
            .await?;
        Ok(result)
    }
}

fn contains_session_placeholder(value: &str) -> bool {
    value.contains("{{session_id}}") || value.contains("${session_id}")
}

/// Sanitize an MCP server name into a safe filename component.
fn sanitize_mcp_log_filename(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .take(96)
        .map(|c| match c {
            c if c.is_ascii_alphanumeric() => c,
            '.' | '_' | '-' => c,
            _ => '_',
        })
        .collect();
    if sanitized.is_empty() {
        "server".into()
    } else {
        sanitized
    }
}

/// Copy an MCP server's stderr to `~/.grok/logs/mcp/<server>.stderr.log`
/// in a background task. Truncated per spawn.
fn drain_mcp_stderr_to_log(server_name: &str, mut stderr: tokio::process::ChildStderr) {
    let log_dir = xai_grok_config::grok_home().join("logs").join("mcp");
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        tracing::warn!("MCP stderr drain: failed to create log dir: {e}");
        return;
    }
    let log_path = log_dir.join(format!(
        "{}.stderr.log",
        sanitize_mcp_log_filename(server_name)
    ));

    let file = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)
    {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                "MCP stderr drain: failed to open {}: {e}",
                log_path.display()
            );
            return;
        }
    };
    let mut file = tokio::fs::File::from_std(file);
    let server_name = server_name.to_string();
    tokio::spawn(async move {
        if let Err(e) = tokio::io::copy(&mut stderr, &mut file).await {
            tracing::warn!("MCP stderr drain '{server_name}': {e}");
        }
    });
}

fn expand_session_id_headers(
    headers: Vec<acp::HttpHeader>,
    session_id: Option<&str>,
) -> Vec<(String, String)> {
    headers
        .into_iter()
        .filter_map(|header| {
            let value = header.value;
            if let Some(session_id) = session_id {
                let expanded = value
                    .replace("{{session_id}}", session_id)
                    .replace("${session_id}", session_id);
                Some((header.name, expanded))
            } else if contains_session_placeholder(&value) {
                None
            } else {
                Some((header.name, value))
            }
        })
        .collect()
}

/// Decide the actual (program, args) to spawn for a stdio MCP server.
///
/// On Windows, npm ships launchers like `npx`/`npm`/`pnpm`/`yarn` as `.cmd`
/// batch shims (there is no `npx.exe`). `CreateProcessW` only appends `.exe`
/// and ignores `PATHEXT`, so `Command::new("npx")` fails with "file not
/// found". We resolve the bare name on `PATH` (honoring `PATHEXT`, via the
/// `resolve` closure) so std spawns the real launcher path (e.g. `npx.cmd`) —
/// std then runs `.cmd`/`.bat` through `cmd.exe` with hardened arg escaping. On
/// non-Windows we never touch the command (verified working). A command
/// containing a path separator is used as-is. The resolved path is returned as
/// an `OsString` so it reaches `Command::new` without a lossy UTF-8 round-trip.
fn plan_stdio_spawn(
    command: &str,
    args: &[String],
    is_windows: bool,
    resolve: impl Fn(&str) -> Option<std::path::PathBuf>,
) -> (OsString, Vec<String>) {
    if is_windows
        && !command.contains('/')
        && !command.contains('\\')
        && let Some(resolved) = resolve(command)
    {
        return (resolved.into_os_string(), args.to_vec());
    }
    (OsString::from(command), args.to_vec())
}

fn is_figma_mcp(server_name: &str, url: &str) -> bool {
    if server_name.eq_ignore_ascii_case("figma") {
        return true;
    }
    // Legacy direct managed name (`grok_com_figma`); newer clients use gateway tools (`managed_mcp_gateway_tools_enabled`).
    const MANAGED_PREFIX: &str = "grok_com_";
    if let (Some(prefix), Some(rest)) = (
        server_name.get(..MANAGED_PREFIX.len()),
        server_name.get(MANAGED_PREFIX.len()..),
    ) && prefix.eq_ignore_ascii_case(MANAGED_PREFIX)
        && rest.eq_ignore_ascii_case("figma")
    {
        return true;
    }
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .is_some_and(|h| h == "figma.com" || h.ends_with(".figma.com"))
}

fn ensure_figma_user_agent(headers: &mut reqwest::header::HeaderMap, server_name: &str, url: &str) {
    if !is_figma_mcp(server_name, url) {
        return;
    }
    if headers.contains_key(reqwest::header::USER_AGENT) {
        return;
    }
    headers.insert(
        reqwest::header::USER_AGENT,
        reqwest::header::HeaderValue::from_static("grok-cli"),
    );
}

fn stdio_path_override(env: &[acp::EnvVariable]) -> Option<&str> {
    env.iter()
        .find(|e| e.name.eq_ignore_ascii_case("PATH"))
        .map(|e| e.value.as_str())
}

pub async fn start_mcp_server(
    mcp_server: acp::McpServer,
    session_id: Option<&str>,
    overrides: Option<&McpClientTimeoutOverrides>,
    meta_config: Option<&McpServerMetaConfig>,
    byo_config: Option<&McpOAuthConfig>,
    event_writer: &xai_file_utils::events::EventWriter,
    mode: OauthInteractivity,
) -> Result<McpClient, McpError> {
    let _per_server_timer = xai_grok_telemetry::instrumentation::timer("mcp_start_one_server");
    match mcp_server {
        acp::McpServer::Stdio(acp::McpServerStdio {
            name,
            command,
            args,
            env,
            ..
        }) => {
            if let Some(mc) = meta_config {
                tracing::info!(server = %name, ?mc, "MCP stdio: meta config override");
            }

            let (startup_timeout, _, _) = McpClient::load_timeouts(overrides, meta_config);
            let command_str = command.to_string_lossy().into_owned();
            let spawn_start = std::time::Instant::now();
            let _stdio_spawn_timer = xai_grok_telemetry::instrumentation::timer("mcp_stdio_spawn");
            let path_override = stdio_path_override(&env);
            let (program, spawn_args) = plan_stdio_spawn(&command_str, &args, cfg!(windows), |c| {
                if let Some(path) = path_override
                    && let Ok(cwd) = std::env::current_dir()
                {
                    which::which_in(c, Some(path), cwd).ok()
                } else {
                    which::which(c).ok()
                }
            });
            let mut cmd = Command::new(&program);
            cmd.kill_on_drop(true).args(&spawn_args);
            for env_variable in &env {
                cmd.env(&env_variable.name, &env_variable.value);
            }
            xai_grok_tools::util::detach_command(&mut cmd);

            let (transport, stderr_handle) =
                SafeTokioChildProcess::spawn(cmd, name.clone(), event_writer.clone()).map_err(
                    |e| {
                        tracing::error!("Failed to spawn MCP server '{}': {}", name, e);
                        xai_grok_telemetry::session_ctx::log_event(
                            xai_grok_telemetry::events::McpServerFailed {
                                server_name: name.clone(),
                                error_type: xai_grok_telemetry::events::McpErrorType::SpawnFailed,
                                duration_ms: spawn_start.elapsed().as_millis() as u64,
                                timeout_sec: startup_timeout,
                            },
                        );
                        McpError::SpawnFailed {
                            server: name.clone(),
                            source: e,
                        }
                    },
                )?;

            tracing::debug!("MCP server '{}' spawned: PID={:?}", name, transport.id());

            if let Some(stderr) = stderr_handle {
                drain_mcp_stderr_to_log(&name, stderr);
            }

            Ok(McpClient::new_stdio(
                name.clone(),
                transport,
                overrides,
                meta_config,
            ))
        }
        acp::McpServer::Http(acp::McpServerHttp {
            name, url, headers, ..
        })
        | acp::McpServer::Sse(acp::McpServerSse {
            name, url, headers, ..
        }) => {
            if let Some(mc) = meta_config {
                tracing::info!(server = %name, %url, ?mc, "MCP http: meta config override");
            }

            let headers = expand_session_id_headers(headers, session_id);
            let http_config = HttpConfig {
                url: url.clone(),
                headers,
            };

            let has_existing_auth = http_config
                .headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("authorization"));

            let auth_prep = if has_existing_auth {
                tracing::debug!(
                    server = %name,
                    "Skipping OAuth discovery: server already has Authorization header"
                );
                HttpOauthPrep::NoOauthSupport
            } else {
                let _auth_discovery_timer =
                    xai_grok_telemetry::instrumentation::timer("mcp_http_auth_discovery");
                match tokio::time::timeout(
                    OAUTH_DISCOVERY_TIMEOUT,
                    discover_and_prepare_auth(&name, &url, mode),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => {
                        tracing::warn!(
                            server = %name,
                            url = %url,
                            ?mode,
                            timeout_secs = OAUTH_DISCOVERY_TIMEOUT.as_secs(),
                            "OAuth discovery timed out"
                        );
                        event_writer.emit(
                            xai_file_utils::events::Event::McpOAuthDiscoveryTimeout {
                                server_name: name.clone(),
                                url: url.clone(),
                            },
                        );
                        HttpOauthPrep::on_probe_failure(mode)
                    }
                }
            };
            match auth_prep {
                HttpOauthPrep::ManagerReady(auth_mgr) => Ok(McpClient::new_http_auth(
                    name.clone(),
                    http_config,
                    auth_mgr,
                    byo_config.cloned(),
                    overrides,
                    meta_config,
                )),
                HttpOauthPrep::NoOauthSupport => Ok(McpClient::new_http(
                    name.clone(),
                    http_config,
                    overrides,
                    meta_config,
                )),
                // Avoid starting an unauthenticated HTTP worker that fatals on server OAuth challenge.
                HttpOauthPrep::NeedsInteractiveLogin => Err(McpError::AuthRequired {
                    server: name.clone(),
                }),
            }
        }
        // TODO(acp-0.10): `McpServer` is #[non_exhaustive]; reject unknown transports.
        other => Err(McpError::ClientError(format!(
            "unsupported MCP server transport: {other:?}"
        ))),
    }
}

pub async fn start_mcp_servers(
    mcp_servers: Vec<acp::McpServer>,
    session_id: Option<&str>,
    overrides_map: &HashMap<String, McpClientTimeoutOverrides>,
    meta_config_map: &McpMetaConfigMap,
    oauth_config_map: &crate::oauth_config::McpOAuthConfigMap,
    event_writer: &xai_file_utils::events::EventWriter,
    mode: OauthInteractivity,
) -> Vec<Result<McpClient, McpError>> {
    let _mcp_start_timer = xai_grok_telemetry::instrumentation::timer("mcp_start_servers");

    if !meta_config_map.is_empty() {
        tracing::info!(
            count = mcp_servers.len(),
            overrides = ?meta_config_map.keys().collect::<Vec<_>>(),
            "Starting MCP servers with meta config"
        );
    }

    futures::stream::iter(mcp_servers)
        .map(|server| {
            let server_name = mcp_server_name(&server);
            let overrides = overrides_map.get(server_name);
            let mc = meta_config_map.get(server_name);
            let byo = oauth_config_map.get(server_name);
            start_mcp_server(server, session_id, overrides, mc, byo, event_writer, mode)
        })
        .buffer_unordered(8)
        .collect::<Vec<_>>()
        .await
}

/// Extract the name from an MCP server enum variant.
pub fn mcp_server_name(server: &acp::McpServer) -> &str {
    match server {
        acp::McpServer::Stdio(stdio) => &stdio.name,
        acp::McpServer::Http(http) => &http.name,
        acp::McpServer::Sse(sse) => &sse.name,
        // TODO(acp-0.10): `McpServer` is #[non_exhaustive].
        _ => "unknown",
    }
}

pub fn mcp_transport_str(server: &acp::McpServer) -> &'static str {
    match server {
        acp::McpServer::Stdio(_) => "stdio",
        acp::McpServer::Http(_) => "http",
        acp::McpServer::Sse(_) => "sse",
        // TODO(acp-0.10): `McpServer` is #[non_exhaustive].
        _ => "unknown",
    }
}

pub fn mcp_target_str(server: &acp::McpServer) -> String {
    match server {
        acp::McpServer::Stdio(acp::McpServerStdio { command, args, .. }) => {
            let cmd = command.to_string_lossy();
            if args.is_empty() {
                cmd.to_string()
            } else {
                format!("{} {}", cmd, args.join(" "))
            }
        }
        acp::McpServer::Http(acp::McpServerHttp { url, .. })
        | acp::McpServer::Sse(acp::McpServerSse { url, .. }) => url.clone(),
        // TODO(acp-0.10): `McpServer` is #[non_exhaustive].
        _ => String::new(),
    }
}

impl McpClient {
    /// Minimal stub for unit tests in dependent crates. Hidden from rustdoc;
    /// not gated behind `#[cfg(test)]` so cross-crate test code can construct
    /// it without the host crate enabling a feature.
    #[doc(hidden)]
    pub fn stub(name: &str) -> Self {
        // Route through the single constructor (so new fields never need
        // touching here), then downgrade to the no-transport placeholder:
        // `Empty` state makes `ensure_initialized` error, and `reconnect =
        // None` makes `reset_transport` return false — i.e. a client that
        // can't reconnect, like a dead Stdio child. Overrides preserve the
        // historical stub timeouts (10s startup / 60s tool).
        let overrides = McpClientTimeoutOverrides {
            startup_timeout_sec: Some(10),
            tool_timeout_sec: Some(60),
            ..Default::default()
        };
        let mut client = Self::new_with_transport(
            name.to_string(),
            PendingTransport::Http(HttpConfig {
                url: String::new(),
                headers: Vec::new(),
            }),
            Some(&overrides),
            None,
            None,
            None,
            None,
        );
        *client.state.get_mut() = ClientState::Empty;
        client.reconnect = None;
        client
    }
}

/// rmcp [`ClientHandler`] used by all MCP transports.
///
/// Replaces the previous bare [`ClientInfo`] handler at the three
/// `client.serve(...)` call sites in [`McpClient::try_handshake`].
/// Plumbs server-pushed notifications through an
/// [`tokio::sync::mpsc::UnboundedSender<McpClientEvent>`] so the
/// session-actor dispatcher can fan them out as ACP
/// `x.ai/mcp/server_status` events.
///
/// ## RPIT, not `#[async_trait]`
///
/// rmcp 2.1's [`ClientHandler`] declares its async methods as
/// return-position `impl Future` (see
/// `~/.cargo/registry/src/.../rmcp-2.1.0/src/handler/client.rs`,
/// lines 202–217). Applying `#[async_trait]` here would produce
/// methods whose signature mismatches the trait, and the impl would
/// not satisfy the bound. The macro path is also unnecessary — the
/// trait already supports `async fn` syntax indirectly via
/// `impl Future<Output = ()> + Send + '_`, which is what we mirror.
///
/// Future contributor reading this: do **not** add `#[async_trait]`.
/// The methods below intentionally return `impl Future` directly.
///
/// ## Notification routing
///
/// `on_tool_list_changed` / `on_resource_list_changed` push an
/// [`McpClientEvent`] into [`Self::notify_tx`]. If the receiver has
/// been dropped (subagent teardown, session shutdown, or the field
/// was `None` to begin with — see [`McpClient::notify_tx`] doc), the
/// send fails silently; rmcp must not see an error from a
/// notification handler or the service loop tears down.
#[derive(Debug)]
pub struct GrokClientHandler {
    /// Static `ClientInfo` returned by [`Self::get_info`]; built once
    /// at handshake time and stored to avoid re-allocating per call.
    info: ClientInfo,
    /// MCP server name this handler is bound to. Cloned into emitted
    /// events so the dispatcher can route per-server.
    server_name: McpServerName,
    /// **Shared** event sink — the same Arc lives on the owning
    /// [`McpClient`]. Mutating the slot via [`McpClient::set_event_tx`]
    /// is observed here on the next read, so wiring the sender
    /// post-handshake is supported without restarting the rmcp
    /// service loop.
    notify_tx: SharedEventTx,
}

impl GrokClientHandler {
    /// Best-effort event emit. Reads the shared `notify_tx` slot on
    /// every call (so the handler picks up any post-handshake wiring
    /// done by [`McpClient::set_event_tx`]). Drops the send error: if
    /// the receiver is gone, the consumer has shut down and there's
    /// nothing useful to do here. Splitting this out keeps the trait
    /// methods short.
    fn emit(&self, ev: McpClientEvent) {
        let sender = self.notify_tx.lock().clone();
        if let Some(tx) = sender {
            let _ = tx.send(ev);
        }
    }
}

impl ClientHandler for GrokClientHandler {
    // NOTE: `async fn` here is sugar for the trait's
    // `-> impl Future<Output = ()> + Send + '_`. We INTENTIONALLY do
    // not use `#[async_trait]` — rmcp 2.1's `ClientHandler` declares
    // its notification methods as return-position `impl Future`, and
    // async_trait would produce a different (incompatible) signature.
    // See the [`GrokClientHandler`] doc-comment for the full RPIT
    // contract.
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.emit(McpClientEvent::ToolsChanged {
            server: self.server_name.clone(),
        });
    }

    async fn on_resource_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.emit(McpClientEvent::ResourcesChanged {
            server: self.server_name.clone(),
        });
    }

    fn get_info(&self) -> ClientInfo {
        self.info.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A single undecodable line on an MCP stdio server's stdout must NOT
    /// collapse the transport: if the decode error surfaced as `None`, the
    /// service would read it as EOF → "Transport closed" → `tools/list` fails
    /// and the connector "shows but doesn't work". The resilient transport
    /// skips the bad line and keeps reading, so a stray stdout log line never
    /// takes the whole server down.
    #[tokio::test]
    async fn resilient_transport_skips_undecodable_line_and_keeps_stream_alive() {
        // `server_out` is the writer half (the fake server's stdout); the
        // transport reads framed JSON-RPC from `client_in`.
        let (mut server_out, client_in) = tokio::io::duplex(64 * 1024);
        let mut transport = ResilientRwTransport::new(
            client_in,
            tokio::io::sink(),
            "fwbuild".to_string(),
            xai_file_utils::events::EventWriter::noop(),
        );

        let valid = r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#;
        // A stray non-JSON log line — the shape that, under rmcp's stock
        // transport, decodes to an error and closes the connection.
        let garbage = "info: fwbuild started, listening on stdio";
        server_out
            .write_all(format!("{valid}\n{garbage}\n{valid}\n").as_bytes())
            .await
            .unwrap();
        // Dropping the writer half signals a clean end-of-stream.
        drop(server_out);

        assert!(
            transport.receive().await.is_some(),
            "first valid message must be received"
        );
        assert!(
            transport.receive().await.is_some(),
            "the undecodable line must be skipped and the next valid message delivered"
        );
        assert!(
            transport.receive().await.is_none(),
            "only a genuine end-of-stream yields None"
        );
    }

    fn make_stdio_server(name: &str, command: &str) -> acp::McpServer {
        acp::McpServer::Stdio(acp::McpServerStdio::new(name, PathBuf::from(command)))
    }

    fn make_http_server(name: &str, url: &str) -> acp::McpServer {
        acp::McpServer::Http(acp::McpServerHttp::new(name, url))
    }

    #[test]
    fn plan_stdio_spawn_windows_resolves_bare_launcher_to_cmd_shim() {
        let args = vec!["-y".to_string(), "@scope/pkg".to_string()];
        let (program, spawn_args) = plan_stdio_spawn("npx", &args, true, |c| {
            assert_eq!(c, "npx");
            Some(PathBuf::from(r"C:\path\npx.cmd"))
        });
        assert_eq!(program, OsString::from(r"C:\path\npx.cmd"));
        assert_eq!(spawn_args, args);
    }

    #[test]
    fn plan_stdio_spawn_windows_unresolved_falls_back_to_raw_command() {
        let args = vec!["-y".to_string(), "@scope/pkg".to_string()];
        let (program, spawn_args) = plan_stdio_spawn("npx", &args, true, |_| None);
        assert_eq!(program, OsString::from("npx"));
        assert_eq!(spawn_args, args);
    }

    #[test]
    fn plan_stdio_spawn_windows_backslash_path_command_used_as_is_without_resolving() {
        let args = vec!["--config".to_string(), "x.json".to_string()];
        let (program, spawn_args) = plan_stdio_spawn(r"C:\tools\server.exe", &args, true, |_| {
            panic!("resolver must not be consulted for a command with a backslash separator")
        });
        assert_eq!(program, OsString::from(r"C:\tools\server.exe"));
        assert_eq!(spawn_args, args);
    }

    #[test]
    fn plan_stdio_spawn_windows_forward_slash_path_command_used_as_is_without_resolving() {
        let args = vec!["--port".to_string(), "8080".to_string()];
        let (program, spawn_args) = plan_stdio_spawn("C:/tools/server.exe", &args, true, |_| {
            panic!("resolver must not be consulted for a command with a forward-slash separator")
        });
        assert_eq!(program, OsString::from("C:/tools/server.exe"));
        assert_eq!(spawn_args, args);
    }

    #[test]
    fn plan_stdio_spawn_non_windows_never_resolves() {
        let args = vec!["-y".to_string(), "pkg".to_string()];
        let (program, spawn_args) = plan_stdio_spawn("npx", &args, false, |_| {
            panic!("resolver must not be consulted on non-Windows")
        });
        assert_eq!(program, OsString::from("npx"));
        assert_eq!(spawn_args, args);
    }

    #[test]
    fn stdio_path_override_matches_path_case_insensitively() {
        let mk = |name: &str, value: &str| acp::EnvVariable::new(name, value);

        let env = vec![mk("FOO", "bar"), mk("Path", r"C:\node")];
        assert_eq!(stdio_path_override(&env), Some(r"C:\node"));

        let env_upper = vec![mk("PATH", "/custom/bin")];
        assert_eq!(stdio_path_override(&env_upper), Some("/custom/bin"));

        let env_none = vec![mk("FOO", "bar")];
        assert_eq!(stdio_path_override(&env_none), None);
    }

    #[test]
    fn is_figma_mcp_matches_name_and_host() {
        assert!(is_figma_mcp("figma", "https://example.com/mcp"));
        assert!(is_figma_mcp("Figma", "https://example.com/mcp"));
        assert!(is_figma_mcp("grok_com_figma", "https://example.com/mcp"));
        assert!(is_figma_mcp("GROK_COM_FIGMA", "https://example.com/mcp"));
        assert!(is_figma_mcp("grok_com_FIGMA", "https://example.com/mcp"));
        assert!(is_figma_mcp("other", "https://mcp.figma.com/mcp"));
        assert!(is_figma_mcp("other", "https://figma.com/mcp"));
        assert!(!is_figma_mcp("linear", "https://mcp.linear.app/mcp"));
        assert!(!is_figma_mcp("figma_extra", "https://example.com/mcp"));
        assert!(!is_figma_mcp("grok_com_linear", "https://example.com/mcp"));
        assert!(!is_figma_mcp("linear", "not-a-url"));
        assert!(!is_figma_mcp("linear", "https://notfigma.com/mcp"));
        assert!(!is_figma_mcp("linear", "https://figma.com.evil/mcp"));
    }

    #[test]
    fn ensure_figma_user_agent_sets_grok_cli_when_missing() {
        let mut headers = reqwest::header::HeaderMap::new();
        ensure_figma_user_agent(&mut headers, "figma", "https://mcp.figma.com/mcp");
        assert_eq!(
            headers.get(reqwest::header::USER_AGENT).unwrap(),
            "grok-cli"
        );

        let mut host_only = reqwest::header::HeaderMap::new();
        ensure_figma_user_agent(&mut host_only, "other", "https://mcp.figma.com/mcp");
        assert_eq!(
            host_only.get(reqwest::header::USER_AGENT).unwrap(),
            "grok-cli"
        );
    }

    #[test]
    fn ensure_figma_user_agent_does_not_overwrite_existing() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static("custom-ua"),
        );
        ensure_figma_user_agent(&mut headers, "figma", "https://mcp.figma.com/mcp");
        assert_eq!(
            headers.get(reqwest::header::USER_AGENT).unwrap(),
            "custom-ua"
        );
    }

    #[test]
    fn ensure_figma_user_agent_skips_non_figma() {
        let mut headers = reqwest::header::HeaderMap::new();
        ensure_figma_user_agent(&mut headers, "linear", "https://mcp.linear.app/mcp");
        assert!(!headers.contains_key(reqwest::header::USER_AGENT));

        let mut invalid_url = reqwest::header::HeaderMap::new();
        ensure_figma_user_agent(&mut invalid_url, "linear", "not-a-url");
        assert!(!invalid_url.contains_key(reqwest::header::USER_AGENT));
    }

    #[cfg(unix)]
    #[test]
    fn safe_stdio_child_drop_without_entered_runtime_reaps_child() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");

        let (transport, pid) = rt.block_on(async {
            let mut cmd = Command::new("sleep");
            cmd.arg("30").kill_on_drop(true);
            xai_grok_tools::util::detach_command(&mut cmd);
            let (transport, _stderr) = SafeTokioChildProcess::spawn(
                cmd,
                "test".to_string(),
                xai_file_utils::events::EventWriter::noop(),
            )
            .expect("spawn test child");
            let pid = transport.id().expect("spawned child pid");
            (transport, pid)
        });

        drop(rt);
        drop(transport);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if !unix_process_exists(pid) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }

        panic!("MCP child process {pid} was not reaped after no-runtime drop");
    }

    #[cfg(unix)]
    fn unix_process_exists(pid: u32) -> bool {
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if result == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }

    #[test]
    fn test_mcp_state_new() {
        let configs = vec![make_stdio_server("test", "/bin/test")];
        let state = McpState::new(configs.clone());

        assert_eq!(state.configs.len(), 1);
        assert!(state.owned_clients.is_empty());
        assert!(!state.is_initialized());
        assert!(!state.is_initializing());
        assert!(!state.has_finished_init());
        assert!(matches!(state.init_progress(), InitProgress::NotStarted));
        assert_eq!(state.generation, 0);
    }

    #[test]
    fn test_mcp_state_update_configs_returns_false_when_unchanged() {
        let configs = vec![make_stdio_server("test", "/bin/test")];
        let mut state = McpState::new(configs.clone());

        // Same configs should return false
        let changed = state.update_configs(configs.clone());
        assert!(!changed);
        assert_eq!(state.generation, 0); // Generation should not change
    }

    #[test]
    fn test_mcp_state_update_configs_returns_true_when_changed() {
        let configs = vec![make_stdio_server("test", "/bin/test")];
        let mut state = McpState::new(configs);

        // Different configs should return true
        let new_configs = vec![make_stdio_server("test2", "/bin/test2")];
        let changed = state.update_configs(new_configs);
        assert!(changed);
        assert_eq!(state.generation, 1); // Generation should increment
    }

    #[test]
    fn test_mcp_state_update_configs_resets_initialized() {
        let configs = vec![make_stdio_server("test", "/bin/test")];
        let mut state = McpState::new(configs);
        // Drive the state machine into Finished{handshaking:{"a"}} so
        // the reset path has both the lifecycle flag AND a per-server
        // entry to clear.
        assert!(state.try_start_init());
        state.mark_servers_initializing(["a".to_string()]);
        state.finish_init();
        assert!(state.has_finished_init());
        assert!(state.is_server_handshaking("a"));

        let new_configs = vec![make_stdio_server("test2", "/bin/test2")];
        let changed = state.update_configs(new_configs);
        assert!(changed);
        // update_configs must drop us back to NotStarted — neither
        // lifecycle flag set nor any per-server progress carried over.
        assert!(!state.is_initialized());
        assert!(!state.is_initializing());
        assert!(!state.has_finished_init());
        assert!(matches!(state.init_progress(), InitProgress::NotStarted));
    }

    #[tokio::test]
    async fn acp_servers_survive_update_configs_clear() {
        use crate::acp_transport::AcpReverseInvoker;
        use std::time::Duration;

        struct NoopInvoker;
        #[async_trait::async_trait]
        impl AcpReverseInvoker for NoopInvoker {
            async fn invoke(
                &self,
                _server_id: &str,
                _message: serde_json::Value,
                _timeout: Duration,
            ) -> Result<serde_json::Value, String> {
                Ok(serde_json::Value::Null)
            }
        }

        let mut state = McpState::new(vec![make_http_server("http-srv", "http://localhost")]);
        state.set_acp_servers(
            vec![AcpServerEntry {
                name: "sdk-tools".to_string(),
                server_id: "srv_0".to_string(),
            }],
            Arc::new(NoopInvoker),
        );
        assert!(state.has_acp_servers());
        assert_eq!(state.build_pending_acp_clients(&HashMap::new()).len(), 1);

        // A config change clears owned clients/configs (proven by the generation bump)
        // but must NOT drop the separately-held acp servers — otherwise the in-process
        // SDK tools would silently vanish on every `update_configs`.
        let changed = state.update_configs(vec![make_http_server("other", "http://other")]);
        assert!(changed);
        assert_eq!(state.generation, 1);
        assert!(
            state.has_acp_servers(),
            "acp servers must survive update_configs"
        );
        let pending = state.build_pending_acp_clients(&HashMap::new());
        assert_eq!(pending.len(), 1, "acp clients rebuild after the clear");
        assert_eq!(pending[0].server_name(), "sdk-tools");
    }

    #[tokio::test]
    async fn acp_overrides_apply_to_built_clients() {
        use crate::acp_transport::AcpReverseInvoker;
        use std::time::Duration;

        struct NoopInvoker;
        #[async_trait::async_trait]
        impl AcpReverseInvoker for NoopInvoker {
            async fn invoke(
                &self,
                _server_id: &str,
                _message: serde_json::Value,
                _timeout: Duration,
            ) -> Result<serde_json::Value, String> {
                Ok(serde_json::Value::Null)
            }
        }

        let mut overrides = HashMap::new();
        overrides.insert(
            "sdk-tools".to_string(),
            McpClientTimeoutOverrides {
                tool_timeout_sec: Some(123),
                ..Default::default()
            },
        );

        let mut state = McpState::new(vec![]);
        state.set_acp_servers(
            vec![AcpServerEntry {
                name: "sdk-tools".to_string(),
                server_id: "srv_0".to_string(),
            }],
            Arc::new(NoopInvoker),
        );

        let pending = state.build_pending_acp_clients(&overrides);
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].tool_timeout_sec(),
            123,
            "config.toml tool_timeout_sec override must reach the SDK client"
        );
    }

    /// In-process SDK (ACP) clients must never get a liveness watcher: the
    /// dispatcher can't recover them (no `configs` entry), so a proactive
    /// `TransportClosed` would evict the client with no recovery. Guards both
    /// the `is_acp` predicate (across transports) and the `arm_liveness_watcher`
    /// self-gate that depends on it. HTTP/stdio must report `false` so they
    /// keep their watchers.
    #[tokio::test]
    async fn acp_clients_are_not_liveness_watched() {
        use crate::acp_transport::AcpReverseInvoker;
        use std::time::Duration;

        struct NoopInvoker;
        #[async_trait::async_trait]
        impl AcpReverseInvoker for NoopInvoker {
            async fn invoke(
                &self,
                _server_id: &str,
                _message: serde_json::Value,
                _timeout: Duration,
            ) -> Result<serde_json::Value, String> {
                Ok(serde_json::Value::Null)
            }
        }

        let acp = McpClient::new_acp(
            "sdk".to_string(),
            "srv_0".to_string(),
            Arc::new(NoopInvoker),
            None,
            None,
        );
        assert!(acp.is_acp());
        assert!(!acp.is_http());

        let http = McpClient::new_http(
            "http".to_string(),
            HttpConfig {
                url: "http://localhost/api/mcp".to_string(),
                headers: vec![],
            },
            None,
            None,
        );
        assert!(!http.is_acp());

        // Stub stands in for a no-transport / Stdio client (reconnect = None).
        assert!(!McpClient::stub("stdio").is_acp());

        // The gate that prevents the evict-on-close bug: arming is a no-op for ACP.
        assert!(
            !Arc::new(acp)
                .arm_liveness_watcher(Duration::from_millis(500))
                .await
        );
    }

    #[test]
    fn test_mark_servers_initializing_clears_prior_init_failure() {
        // A server that failed a previous init is recorded in `init_failed`
        // (so the status snapshot reports it Unavailable). Starting a fresh
        // init attempt for that server must clear the failure flag so a
        // successful retry can surface as Ready again.
        let mut state = McpState::new(vec![make_stdio_server("a", "/bin/a")]);
        state.init_failed.insert("a".to_string(), String::new());
        state.init_failed.insert("b".to_string(), String::new());

        state.mark_servers_initializing(["a".to_string()]);

        assert!(
            !state.init_failed.contains_key("a"),
            "fresh init attempt must clear the prior failure for that server",
        );
        assert!(
            state.init_failed.contains_key("b"),
            "servers not in this init attempt must keep their failure flag",
        );
    }

    #[test]
    fn test_record_init_failure_keeps_auth_and_init_failed_disjoint() {
        let mut state = McpState::new(vec![make_stdio_server("a", "/bin/a")]);

        // Auth failures are owned by `auth_required` only — never `init_failed` —
        // so a later successful authentication (which clears `auth_required` and
        // registers tools) is not left stuck as Unavailable with zero tools.
        state.record_init_failure("auth-srv", true, None);
        assert!(state.auth_required.contains("auth-srv"));
        assert!(
            !state.init_failed.contains_key("auth-srv"),
            "auth-required failures must not also be flagged init_failed",
        );

        // Non-auth failures (handshake/`tools/list` error or timeout) → init_failed,
        // and their cause is retained for the model-facing reminder.
        state.record_init_failure(
            "dead-srv",
            false,
            Some("tools/list failed: boom".to_string()),
        );
        assert!(!state.auth_required.contains("dead-srv"));
        assert_eq!(
            state.init_failed.get("dead-srv").map(String::as_str),
            Some("tools/list failed: boom"),
        );

        // A fresh init attempt clears the failure entry and its cause.
        state.mark_servers_initializing(["dead-srv".to_string()]);
        assert!(!state.init_failed.contains_key("dead-srv"));
    }

    #[test]
    fn test_clear_init_failed_removes_entry() {
        let mut state = McpState::new(vec![make_stdio_server("a", "/bin/a")]);
        state.record_init_failure("dead-srv", false, Some("boom".to_string()));
        assert!(state.init_failed.contains_key("dead-srv"));

        // Symmetric with record_init_failure: the reactive re-auth path clears
        // a prior failure so a recovered server is not stuck Unavailable.
        state.clear_init_failed("dead-srv");
        assert!(!state.init_failed.contains_key("dead-srv"));
        // Idempotent: clearing an absent entry is a no-op.
        state.clear_init_failed("never-seen");
    }

    #[test]
    fn test_mcp_state_update_configs_increments_generation() {
        let mut state = McpState::new(vec![]);

        // Each change should increment generation
        state.update_configs(vec![make_stdio_server("a", "/bin/a")]);
        assert_eq!(state.generation, 1);

        state.update_configs(vec![make_stdio_server("b", "/bin/b")]);
        assert_eq!(state.generation, 2);

        state.update_configs(vec![make_stdio_server("c", "/bin/c")]);
        assert_eq!(state.generation, 3);
    }

    #[test]
    fn test_mcp_servers_equal_empty_lists() {
        let a: Vec<acp::McpServer> = vec![];
        let b: Vec<acp::McpServer> = vec![];
        assert!(mcp_servers_equal(&a, &b));
    }

    #[test]
    fn test_mcp_servers_equal_identical_configs() {
        let a = vec![make_stdio_server("test", "/bin/test")];
        let b = vec![make_stdio_server("test", "/bin/test")];
        assert!(mcp_servers_equal(&a, &b));
    }

    #[test]
    fn test_mcp_servers_equal_different_names() {
        let a = vec![make_stdio_server("test1", "/bin/test")];
        let b = vec![make_stdio_server("test2", "/bin/test")];
        assert!(!mcp_servers_equal(&a, &b));
    }

    #[test]
    fn test_mcp_servers_equal_different_lengths() {
        let a = vec![make_stdio_server("test", "/bin/test")];
        let b = vec![
            make_stdio_server("test", "/bin/test"),
            make_stdio_server("test2", "/bin/test2"),
        ];
        assert!(!mcp_servers_equal(&a, &b));
    }

    #[test]
    fn test_mcp_servers_equal_different_types() {
        let a = vec![make_stdio_server("test", "/bin/test")];
        let b = vec![make_http_server("test", "http://localhost")];
        assert!(!mcp_servers_equal(&a, &b));
    }

    #[test]
    fn test_mcp_servers_equal_order_matters() {
        let a = vec![
            make_stdio_server("a", "/bin/a"),
            make_stdio_server("b", "/bin/b"),
        ];
        let b = vec![
            make_stdio_server("b", "/bin/b"),
            make_stdio_server("a", "/bin/a"),
        ];
        // Order matters since we're comparing JSON serialization
        assert!(!mcp_servers_equal(&a, &b));
    }

    #[test]
    fn test_try_start_init_prevents_concurrent_init() {
        let mut state = McpState::new(vec![make_stdio_server("test", "/bin/test")]);

        // First call should succeed
        assert!(state.try_start_init());
        assert!(state.is_initializing());
        assert!(!state.is_initialized());

        // Second call should fail (already initializing)
        assert!(!state.try_start_init());
    }

    #[test]
    fn test_try_start_init_fails_when_initialized() {
        let mut state = McpState::new(vec![make_stdio_server("test", "/bin/test")]);
        // Drive to Finished{empty} via the typed API.
        assert!(state.try_start_init());
        state.finish_init();
        assert!(state.is_initialized());

        // Second `try_start_init` must be rejected: we're already done.
        assert!(!state.try_start_init());
        assert!(!state.is_initializing());
        assert!(state.is_initialized(), "is_initialized stays true");
    }

    #[test]
    fn test_finish_init_clears_initializing() {
        let mut state = McpState::new(vec![make_stdio_server("test", "/bin/test")]);

        state.try_start_init();
        assert!(state.is_initializing());
        assert!(!state.is_initialized());

        state.finish_init();
        assert!(!state.is_initializing());
        assert!(state.is_initialized());
    }

    #[test]
    fn test_cancel_init_clears_initializing() {
        let mut state = McpState::new(vec![make_stdio_server("test", "/bin/test")]);

        state.try_start_init();
        assert!(state.is_initializing());

        state.cancel_init();
        assert!(!state.is_initializing());
        assert!(!state.is_initialized()); // Should NOT be marked as initialized
    }

    #[test]
    fn test_update_configs_resets_initializing() {
        let mut state = McpState::new(vec![make_stdio_server("test", "/bin/test")]);
        state.try_start_init();
        assert!(state.is_initializing());

        // Updating configs should reset initializing flag
        state.update_configs(vec![make_stdio_server("test2", "/bin/test2")]);
        assert!(!state.is_initializing());
        assert!(!state.is_initialized());
    }

    #[test]
    fn test_parse_mcp_meta_config_with_tool_timeouts_ms() {
        let meta = serde_json::json!({
            "mcpConfig": {
                "github": {
                    "toolTimeoutMs": 60000,
                    "toolTimeoutsMs": {
                        "create_issue": 120000,
                        "search": 30000
                    }
                }
            }
        })
        .as_object()
        .cloned()
        .unwrap();
        let map = parse_mcp_meta_config(Some(&meta));
        let github = map.get("github").unwrap();
        assert_eq!(github.tool_timeout_ms, Some(60000));
        let tt = github.tool_timeouts_ms.as_ref().unwrap();
        assert_eq!(tt.get("create_issue"), Some(&120000));
        assert_eq!(tt.get("search"), Some(&30000));
    }

    #[test]
    fn test_parse_mcp_meta_config_without_tool_timeouts_ms() {
        let meta = serde_json::json!({
            "mcpConfig": {
                "github": {
                    "toolTimeoutMs": 60000
                }
            }
        })
        .as_object()
        .cloned()
        .unwrap();
        let map = parse_mcp_meta_config(Some(&meta));
        let github = map.get("github").unwrap();
        assert_eq!(github.tool_timeout_ms, Some(60000));
        assert!(github.tool_timeouts_ms.is_none());
        assert!(github.expose_image_base64.is_none());
    }

    /// Locks in the `exposeImageBase64` camelCase wire-format contract.
    #[test]
    fn test_parse_mcp_meta_config_with_expose_image_base64() {
        let meta = serde_json::json!({
            "mcpConfig": {
                "grafana": { "exposeImageBase64": true },
                "linear":  { "exposeImageBase64": false },
            }
        })
        .as_object()
        .cloned()
        .unwrap();
        let map = parse_mcp_meta_config(Some(&meta));
        assert_eq!(map.get("grafana").unwrap().expose_image_base64, Some(true));
        assert_eq!(map.get("linear").unwrap().expose_image_base64, Some(false));
    }

    #[test]
    fn test_tool_timeout_for_returns_per_tool_override() {
        let mut tool_timeouts = HashMap::new();
        tool_timeouts.insert("create_issue".to_string(), 120u64);
        tool_timeouts.insert("search".to_string(), 30u64);

        let overrides = McpClientTimeoutOverrides {
            startup_timeout_sec: Some(10),
            tool_timeout_sec: Some(60),
            tool_timeouts: Some(tool_timeouts),
            ..Default::default()
        };
        let client = McpClient::new_http(
            "github".to_string(),
            HttpConfig {
                url: String::new(),
                headers: vec![],
            },
            Some(&overrides),
            None,
        );

        // Per-tool overrides
        assert_eq!(client.tool_timeout_for("create_issue"), 120);
        assert_eq!(client.tool_timeout_for("search"), 30);
        // Falls back to server-level default
        assert_eq!(client.tool_timeout_for("list_repos"), 60);
        assert_eq!(client.tool_timeout_for(""), 60);
    }

    #[test]
    fn test_tool_timeout_for_empty_map_returns_default() {
        let overrides = McpClientTimeoutOverrides {
            startup_timeout_sec: Some(10),
            tool_timeout_sec: Some(45),
            ..Default::default()
        };
        let client = McpClient::new_http(
            "test".to_string(),
            HttpConfig {
                url: String::new(),
                headers: vec![],
            },
            Some(&overrides),
            None,
        );

        // All tools should get the server-level default
        assert_eq!(client.tool_timeout_for("any_tool"), 45);
        assert_eq!(client.tool_timeout_sec(), 45);
    }

    #[test]
    fn test_load_timeouts_startup_precedence() {
        // No override -> the standalone default (env/config resolved by the shell).
        assert_eq!(
            McpClient::load_timeouts(None, None).0,
            DEFAULT_STARTUP_TIMEOUT_SECS
        );

        // A per-server `startup_timeout_sec` (injected by the shell) wins over the default...
        let overrides = McpClientTimeoutOverrides {
            startup_timeout_sec: Some(7),
            ..Default::default()
        };
        assert_eq!(McpClient::load_timeouts(Some(&overrides), None).0, 7);

        // ...and `_meta.startup_timeout_ms` wins over that.
        let meta = McpServerMetaConfig {
            startup_timeout_ms: Some(12_000),
            ..Default::default()
        };
        assert_eq!(
            McpClient::load_timeouts(Some(&overrides), Some(&meta)).0,
            12
        );
    }

    #[test]
    fn test_update_configs_diff_no_change() {
        let configs = vec![make_stdio_server("test", "/bin/test")];
        let mut state = McpState::new(configs.clone());
        assert!(state.update_configs_diff(configs).is_none());
        assert_eq!(state.generation, 0);
    }

    #[test]
    fn test_update_configs_diff_added() {
        let configs = vec![make_stdio_server("a", "/bin/a")];
        let mut state = McpState::new(configs);

        let new_configs = vec![
            make_stdio_server("a", "/bin/a"),
            make_stdio_server("b", "/bin/b"),
        ];
        let diff = state
            .update_configs_diff(new_configs)
            .expect("should detect change");
        assert_eq!(diff.retained, vec!["a"]);
        assert_eq!(diff.added, vec!["b"]);
        assert!(diff.removed.is_empty());
        assert_eq!(state.generation, 1);
    }

    #[test]
    fn test_update_configs_diff_removed() {
        let configs = vec![
            make_stdio_server("a", "/bin/a"),
            make_stdio_server("b", "/bin/b"),
        ];
        let mut state = McpState::new(configs);

        let new_configs = vec![make_stdio_server("a", "/bin/a")];
        let diff = state
            .update_configs_diff(new_configs)
            .expect("should detect change");
        assert_eq!(diff.retained, vec!["a"]);
        assert!(diff.added.is_empty());
        assert_eq!(diff.removed, vec!["b"]);
    }

    #[test]
    fn test_update_configs_diff_changed() {
        let configs = vec![make_stdio_server("a", "/bin/a")];
        let mut state = McpState::new(configs);

        let new_configs = vec![make_stdio_server("a", "/bin/a_v2")];
        let diff = state
            .update_configs_diff(new_configs)
            .expect("should detect change");
        assert!(diff.retained.is_empty());
        assert_eq!(diff.added, vec!["a"]);
        assert_eq!(diff.removed, vec!["a"]);
    }

    #[test]
    fn test_update_configs_diff_auth_required_cleanup() {
        let configs = vec![
            make_stdio_server("keep", "/bin/keep"),
            make_stdio_server("remove", "/bin/remove"),
        ];
        let mut state = McpState::new(configs);
        state.auth_required.insert("remove".to_string());
        state.auth_required.insert("keep".to_string());

        let new_configs = vec![make_stdio_server("keep", "/bin/keep")];
        let diff = state
            .update_configs_diff(new_configs)
            .expect("should detect change");
        assert_eq!(diff.retained, vec!["keep"]);
        assert_eq!(diff.removed, vec!["remove"]);
        assert!(state.auth_required.contains("keep"));
        assert!(!state.auth_required.contains("remove"));
    }

    #[test]
    fn test_update_configs_diff_empty_to_nonempty() {
        let mut state = McpState::new(vec![]);
        let new_configs = vec![make_stdio_server("a", "/bin/a")];
        let diff = state
            .update_configs_diff(new_configs)
            .expect("should detect change");
        assert!(diff.retained.is_empty());
        assert_eq!(diff.added, vec!["a"]);
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn test_update_configs_diff_nonempty_to_empty() {
        let configs = vec![make_stdio_server("a", "/bin/a")];
        let mut state = McpState::new(configs);
        let diff = state
            .update_configs_diff(vec![])
            .expect("should detect change");
        assert!(diff.retained.is_empty());
        assert!(diff.added.is_empty());
        assert_eq!(diff.removed, vec!["a"]);
    }

    /// Two MCP servers exposing a tool with the same raw name must produce
    /// `McpErasedTool` instances with **distinct** `ToolId`s (qualified with
    /// the server name). Regression test for a bug where `McpErasedTool::id()`
    /// returned the unqualified name, causing the second registration to
    /// silently overwrite the first in the `LocalRegistry`.
    #[test]
    fn test_mcp_erased_tool_id_is_qualified() {
        use xai_tool_runtime::Tool;

        let mcp_state = Arc::new(Mutex::new(McpState::new(vec![])));

        let tool_a = McpErasedTool {
            tool: McpTool::new(
                "SearchUsers".to_string(),
                "Search users".to_string(),
                "calendar".to_string(),
                Arc::clone(&mcp_state),
                serde_json::json!({"type": "object"}),
                None,
            ),
        };
        let tool_b = McpErasedTool {
            tool: McpTool::new(
                "SearchUsers".to_string(),
                "Search users".to_string(),
                "teams".to_string(),
                Arc::clone(&mcp_state),
                serde_json::json!({"type": "object"}),
                None,
            ),
        };

        let id_a = tool_a.id();
        let id_b = tool_b.id();

        // IDs must be qualified with the server name.
        assert_eq!(id_a.as_str(), "calendar__SearchUsers");
        assert_eq!(id_b.as_str(), "teams__SearchUsers");

        // And therefore distinct.
        assert_ne!(id_a, id_b);
    }

    /// Registering two MCP tools with the same raw name from different servers
    /// into a `LocalRegistry` must preserve both entries (no silent overwrite).
    #[test]
    fn test_same_raw_name_different_servers_no_local_registry_collision() {
        use xai_computer_hub_sdk::LocalRegistry;
        use xai_tool_runtime::Tool;

        let mcp_state = Arc::new(Mutex::new(McpState::new(vec![])));
        let registry = LocalRegistry::new();

        let tool_a = McpErasedTool {
            tool: McpTool::new(
                "SearchUsers".to_string(),
                "Search users on calendar".to_string(),
                "calendar".to_string(),
                Arc::clone(&mcp_state),
                serde_json::json!({"type": "object"}),
                None,
            ),
        };
        let tool_b = McpErasedTool {
            tool: McpTool::new(
                "SearchUsers".to_string(),
                "Search users on teams".to_string(),
                "teams".to_string(),
                Arc::clone(&mcp_state),
                serde_json::json!({"type": "object"}),
                None,
            ),
        };

        let id_a = tool_a.id();
        let id_b = tool_b.id();

        // First registration should not displace anything.
        let displaced_a = registry.register(tool_a);
        assert!(
            displaced_a.is_none(),
            "first registration should not displace"
        );

        // Second registration should also not displace anything (distinct IDs).
        let displaced_b = registry.register(tool_b);
        assert!(
            displaced_b.is_none(),
            "second registration must not overwrite first"
        );

        // Both tools must be independently resolvable.
        assert!(
            registry.find(&id_a).is_some(),
            "calendar tool must be found"
        );
        assert!(registry.find(&id_b).is_some(), "teams tool must be found");
        assert_eq!(registry.len(), 2);
    }

    fn make_test_client(name: &str) -> Arc<McpClient> {
        // Same shape as the no-transport placeholder.
        Arc::new(McpClient::stub(name))
    }

    #[test]
    fn test_shared_mcp_pool_from_empty_state() {
        let state = McpState::new(vec![]);
        let pool = SharedMcpPool::from_state(&state);
        assert_eq!(pool.len(), 0);
        assert_eq!(pool.server_names().count(), 0);
        assert!(pool.configs().is_empty());
        assert!(pool.meta_config_map().is_empty());
        assert!(pool.get_client("anything").is_none());
    }

    #[test]
    fn test_shared_mcp_pool_len_matches_client_count() {
        let mut state = McpState::new(vec![]);
        for name in ["alpha", "beta", "gamma"] {
            state
                .owned_clients
                .insert(name.to_string(), make_test_client(name));
        }
        let pool = SharedMcpPool::from_state(&state);
        assert_eq!(pool.len(), 3);
        assert_eq!(pool.len(), pool.server_names().count());
    }

    #[test]
    fn test_shared_mcp_pool_snapshot_shares_arc_clients() {
        let mut state = McpState::new(vec![make_stdio_server("github", "/bin/gh")]);
        let client = make_test_client("github");
        state
            .owned_clients
            .insert("github".to_string(), Arc::clone(&client));

        let pool = SharedMcpPool::from_state(&state);
        let pool_client = pool.get_client("github").expect("should find client");

        // Must point to the same allocation (shared transport)
        assert!(Arc::ptr_eq(&client, pool_client));
    }

    #[test]
    fn test_shared_mcp_pool_get_client_missing() {
        let mut state = McpState::new(vec![]);
        state
            .owned_clients
            .insert("a".to_string(), make_test_client("a"));
        let pool = SharedMcpPool::from_state(&state);

        assert!(pool.get_client("a").is_some());
        assert!(pool.get_client("nonexistent").is_none());
        assert!(pool.get_client("").is_none());
    }

    #[test]
    fn test_shared_mcp_pool_server_names() {
        let mut state = McpState::new(vec![]);
        for name in ["alpha", "beta", "gamma"] {
            state
                .owned_clients
                .insert(name.to_string(), make_test_client(name));
        }

        let pool = SharedMcpPool::from_state(&state);
        let mut names: Vec<&str> = pool.server_names().collect();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn test_shared_mcp_pool_snapshot_independent_of_state_mutations() {
        let mut state = McpState::new(vec![make_stdio_server("srv", "/bin/srv")]);
        state
            .owned_clients
            .insert("srv".to_string(), make_test_client("srv"));

        let pool = SharedMcpPool::from_state(&state);

        // Mutate state after snapshot
        state.owned_clients.clear();
        state.configs.clear();

        // Pool retains original data
        assert_eq!(pool.server_names().count(), 1);
        assert!(pool.get_client("srv").is_some());
        assert_eq!(pool.configs().len(), 1);
    }

    #[test]
    fn test_shared_mcp_pool_meta_config_preserved() {
        let mut meta = McpMetaConfigMap::new();
        meta.insert(
            "github".to_string(),
            McpServerMetaConfig {
                startup_timeout_ms: Some(5000),
                tool_timeout_ms: Some(120000),
                tool_timeouts_ms: None,
                expose_image_base64: None,
            },
        );
        let state =
            McpState::new_with_meta(vec![make_http_server("github", "http://gh.local")], meta);
        let pool = SharedMcpPool::from_state(&state);

        let mc = pool
            .meta_config_map()
            .get("github")
            .expect("should have meta config");
        assert_eq!(mc.startup_timeout_ms, Some(5000));
        assert_eq!(mc.tool_timeout_ms, Some(120000));
    }

    #[test]
    fn test_shared_mcp_pool_clone_shares_arcs() {
        let mut state = McpState::new(vec![]);
        let client = make_test_client("svc");
        state
            .owned_clients
            .insert("svc".to_string(), Arc::clone(&client));

        let pool = SharedMcpPool::from_state(&state);
        let pool2 = pool.clone();

        // Both clones share the same Arc<McpClient>
        let c1 = pool.get_client("svc").unwrap();
        let c2 = pool2.get_client("svc").unwrap();
        assert!(Arc::ptr_eq(c1, c2));
    }

    // ── owned/shared split behavioral tests ─────────────────────────

    #[test]
    fn test_get_client_owned_overrides_shared() {
        let mut state = McpState::new(vec![]);
        let shared = make_test_client("srv");
        let owned = make_test_client("srv");
        state
            .shared_clients
            .insert("srv".to_string(), Arc::clone(&shared));
        state
            .owned_clients
            .insert("srv".to_string(), Arc::clone(&owned));

        let got = state.get_client("srv").unwrap();
        assert!(Arc::ptr_eq(got, &owned));
        assert!(!Arc::ptr_eq(got, &shared));
    }

    #[test]
    fn test_get_client_falls_through_to_shared() {
        let mut state = McpState::new(vec![]);
        let shared = make_test_client("srv");
        state
            .shared_clients
            .insert("srv".to_string(), Arc::clone(&shared));

        let got = state.get_client("srv").unwrap();
        assert!(Arc::ptr_eq(got, &shared));
        assert!(state.get_client("missing").is_none());
    }

    #[test]
    fn test_all_clients_deduplicates_shared_by_owned() {
        let mut state = McpState::new(vec![]);
        state
            .owned_clients
            .insert("a".to_string(), make_test_client("a"));
        state
            .shared_clients
            .insert("a".to_string(), make_test_client("a-shared"));
        state
            .shared_clients
            .insert("b".to_string(), make_test_client("b-shared"));

        let all: Vec<_> = state.all_clients().map(|(n, _)| n.as_str()).collect();
        // "a" appears once (from owned), "b" from shared
        assert_eq!(all.iter().filter(|&&n| n == "a").count(), 1);
        assert!(all.contains(&"b"));
        assert_eq!(all.len(), 2);

        // The "a" entry must be the owned client, not the shared one
        let (_, a_client) = state.all_clients().find(|(n, _)| *n == "a").unwrap();
        assert!(Arc::ptr_eq(a_client, state.owned_clients.get("a").unwrap()));
    }

    #[test]
    fn test_import_shared_clients_skips_config_collisions() {
        // Child has a config entry named "github" — importing a shared
        // client with the same name must be skipped.
        let mut state = McpState::new(vec![make_stdio_server("github", "/bin/gh")]);
        let mut pool_clients = HashMap::new();
        pool_clients.insert("github".to_string(), make_test_client("github"));
        pool_clients.insert("linear".to_string(), make_test_client("linear"));
        let pool = SharedMcpPool {
            clients: pool_clients,
            configs: vec![],
            meta_config_map: McpMetaConfigMap::new(),
        };

        state.import_shared_clients(&pool);

        assert!(
            !state.shared_clients.contains_key("github"),
            "github should be skipped — collides with child config"
        );
        assert!(
            state.shared_clients.contains_key("linear"),
            "linear should be imported — no collision"
        );
    }

    #[test]
    fn test_update_configs_preserves_shared_clients() {
        let mut state = McpState::new(vec![make_stdio_server("old", "/bin/old")]);
        state
            .owned_clients
            .insert("old".to_string(), make_test_client("old"));
        let shared = make_test_client("inherited");
        state
            .shared_clients
            .insert("inherited".to_string(), Arc::clone(&shared));

        let changed = state.update_configs(vec![make_stdio_server("new", "/bin/new")]);

        assert!(changed);
        assert!(state.owned_clients.is_empty(), "owned should be cleared");
        assert_eq!(state.shared_clients.len(), 1, "shared should be untouched");
        assert!(Arc::ptr_eq(
            state.shared_clients.get("inherited").unwrap(),
            &shared
        ));
    }

    #[test]
    fn test_update_configs_diff_preserves_shared_clients() {
        let mut state = McpState::new(vec![
            make_stdio_server("keep", "/bin/keep"),
            make_stdio_server("drop", "/bin/drop"),
        ]);
        state
            .owned_clients
            .insert("keep".to_string(), make_test_client("keep"));
        state
            .owned_clients
            .insert("drop".to_string(), make_test_client("drop"));
        let shared = make_test_client("inherited");
        state
            .shared_clients
            .insert("inherited".to_string(), Arc::clone(&shared));

        // New config removes "drop", keeps "keep"
        let diff = state
            .update_configs_diff(vec![make_stdio_server("keep", "/bin/keep")])
            .expect("configs changed");

        assert!(diff.removed.contains(&"drop".to_string()));
        assert!(diff.retained.contains(&"keep".to_string()));
        assert!(!state.owned_clients.contains_key("drop"));
        assert!(state.owned_clients.contains_key("keep"));
        // Shared clients must be completely untouched
        assert!(Arc::ptr_eq(
            state.shared_clients.get("inherited").unwrap(),
            &shared
        ));
    }

    #[test]
    fn test_from_state_captures_both_owned_and_shared() {
        let mut state = McpState::new(vec![]);
        let owned = make_test_client("owned-srv");
        let shared = make_test_client("shared-srv");
        state
            .owned_clients
            .insert("owned-srv".to_string(), Arc::clone(&owned));
        state
            .shared_clients
            .insert("shared-srv".to_string(), Arc::clone(&shared));

        let pool = SharedMcpPool::from_state(&state);

        assert!(Arc::ptr_eq(pool.get_client("owned-srv").unwrap(), &owned));
        assert!(Arc::ptr_eq(pool.get_client("shared-srv").unwrap(), &shared));
        assert_eq!(pool.server_names().count(), 2);
    }

    #[test]
    fn test_retain_clients_keeps_matching() {
        let mut state = McpState::new(vec![]);
        for name in ["github", "linear", "slack"] {
            state
                .owned_clients
                .insert(name.to_string(), make_test_client(name));
        }
        let mut pool = SharedMcpPool::from_state(&state);

        pool.retain_clients(|name| name == "github" || name == "slack");

        assert!(pool.get_client("github").is_some());
        assert!(pool.get_client("slack").is_some());
        assert!(pool.get_client("linear").is_none());
        assert_eq!(pool.server_names().count(), 2);
    }

    #[test]
    fn test_retain_clients_remove_all() {
        let mut state = McpState::new(vec![]);
        state
            .owned_clients
            .insert("srv".to_string(), make_test_client("srv"));
        let mut pool = SharedMcpPool::from_state(&state);

        pool.retain_clients(|_| false);

        assert_eq!(pool.server_names().count(), 0);
        assert!(pool.get_client("srv").is_none());
    }

    #[test]
    fn test_retain_clients_keep_all() {
        let mut state = McpState::new(vec![]);
        for name in ["a", "b", "c"] {
            state
                .owned_clients
                .insert(name.to_string(), make_test_client(name));
        }
        let mut pool = SharedMcpPool::from_state(&state);

        pool.retain_clients(|_| true);

        assert_eq!(pool.server_names().count(), 3);
    }

    #[test]
    fn test_retain_clients_preserves_arc_identity() {
        let mut state = McpState::new(vec![]);
        let client = make_test_client("keep");
        state
            .owned_clients
            .insert("keep".to_string(), Arc::clone(&client));
        state
            .owned_clients
            .insert("drop".to_string(), make_test_client("drop"));
        let mut pool = SharedMcpPool::from_state(&state);

        pool.retain_clients(|name| name == "keep");

        assert!(Arc::ptr_eq(pool.get_client("keep").unwrap(), &client));
    }

    fn make_mcp_tool(server_name: &str, name: &str) -> McpTool {
        McpTool::new(
            name.to_string(),
            "test desc".to_string(),
            server_name.to_string(),
            Arc::new(Mutex::new(McpState::new(vec![]))),
            serde_json::json!({}),
            None,
        )
    }

    #[test]
    fn qualified_mcp_name_parser_accepts_structurally_valid_tool_ids() {
        for (name, expected) in [
            ("linear__list_issues", ("linear", "list_issues")),
            ("123__lookup", ("123", "lookup")),
            ("server:scope__tool", ("server:scope", "tool")),
        ] {
            let (id, server, tool) = parse_mcp_qualified_name(name).expect("valid qualified ID");
            assert_eq!(id.as_str(), name);
            assert_eq!((server, tool), expected);
            assert_eq!(
                parse_mcp_tool_name(name),
                Some((expected.0.to_owned(), expected.1.to_owned()))
            );
        }
    }

    #[test]
    fn qualified_mcp_name_parser_rejects_malformed_names() {
        for name in [
            "server__part__tool",
            "server__tool__part",
            "foo___bar",
            "foo____bar",
            "__tool",
            "server__",
            "server",
            "",
            "server__bad.tool",
        ] {
            assert!(
                parse_mcp_qualified_name(name).is_none(),
                "unexpectedly accepted {name:?}"
            );
        }
    }

    #[test]
    fn into_registration_validates_qualified_name() {
        let registration = make_mcp_tool("linear", "list_issues")
            .into_registration()
            .expect("should register");
        assert_eq!(registration.name, "linear__list_issues");

        for (server, tool) in [
            ("server__part", "tool"),
            ("server", "tool__part"),
            ("foo_", "bar"),
            ("foo", "_bar"),
            ("foo_", "_bar"),
            ("", "tool"),
            ("server", ""),
        ] {
            assert!(
                make_mcp_tool(server, tool).into_registration().is_none(),
                "unexpectedly registered {server:?} and {tool:?}"
            );
        }
    }

    #[test]
    fn into_registration_preserves_provider_name_policy() {
        for qualified in ["123__lookup", "server:scope__tool"] {
            assert!(parse_mcp_qualified_name(qualified).is_some());
            let (server, tool) = qualified.split_once("__").unwrap();
            assert!(make_mcp_tool(server, tool).into_registration().is_none());
        }

        let server_61 = format!("a{}", "b".repeat(60));
        let server_62 = format!("a{}", "b".repeat(61));
        let valid_64 = format!("{server_61}__b");
        let invalid_65 = format!("{server_62}__b");
        assert_eq!(valid_64.len(), 64);
        assert_eq!(invalid_65.len(), 65);
        assert!(parse_mcp_qualified_name(&valid_64).is_some());
        assert!(parse_mcp_qualified_name(&invalid_65).is_some());
        assert!(make_mcp_tool(&server_61, "b").into_registration().is_some());
        assert!(make_mcp_tool(&server_62, "b").into_registration().is_none());
    }

    // ── is_retriable_transport_error tests ───────────────────────────

    #[test]
    fn test_is_retriable_transport_closed() {
        assert!(is_retriable_transport_error(&ServiceError::TransportClosed));
    }

    #[test]
    fn test_is_retriable_transport_send() {
        let err = ServiceError::TransportSend(rmcp::transport::DynamicTransportError::from_parts(
            "test",
            std::any::TypeId::of::<()>(),
            Box::new(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "connection reset",
            )),
        ));
        assert!(is_retriable_transport_error(&err));
    }

    #[test]
    fn test_not_retriable_unexpected_response() {
        assert!(!is_retriable_transport_error(
            &ServiceError::UnexpectedResponse
        ));
    }

    #[test]
    fn test_not_retriable_cancelled() {
        assert!(!is_retriable_transport_error(&ServiceError::Cancelled {
            reason: Some("shutdown".to_string()),
        }));
    }

    #[test]
    fn test_not_retriable_timeout() {
        assert!(!is_retriable_transport_error(&ServiceError::Timeout {
            timeout: std::time::Duration::from_secs(30),
        }));
    }

    fn mcp_service_err(code: i32) -> ServiceError {
        ServiceError::McpError(rmcp::ErrorData::new(
            rmcp::model::ErrorCode(code),
            "boom",
            None,
        ))
    }

    #[test]
    fn should_recover_mcp_error_recovers_everything_outside_excluded_set() {
        assert!(should_recover_mcp_error(-32603));
        assert!(should_recover_mcp_error(-32002));
        assert!(should_recover_mcp_error(-32000));
        assert!(should_recover_mcp_error(-32099));
        assert!(should_recover_mcp_error(-32100));
        assert!(should_recover_mcp_error(0));
        assert!(should_recover_mcp_error(1));
        assert!(should_recover_mcp_error(i32::MIN));
        assert!(should_recover_mcp_error(i32::MAX));
    }

    #[test]
    fn should_recover_mcp_error_skips_deterministic_client_errors() {
        assert!(!should_recover_mcp_error(-32700));
        assert!(!should_recover_mcp_error(-32600));
        assert!(!should_recover_mcp_error(-32601));
        assert!(!should_recover_mcp_error(-32602));
    }

    #[test]
    fn should_recover_service_error_http_mcperror_recoverable() {
        assert!(should_recover_service_error(
            &mcp_service_err(-32603),
            true,
            false,
        ));
    }

    #[test]
    fn should_recover_service_error_http_mcperror_invalid_params_skipped() {
        assert!(!should_recover_service_error(
            &mcp_service_err(-32602),
            true,
            false,
        ));
    }

    #[test]
    fn should_recover_service_error_stdio_mcperror_not_recovered() {
        assert!(!should_recover_service_error(
            &mcp_service_err(-32603),
            false,
            false,
        ));
    }

    #[test]
    fn should_recover_service_error_mcperror_at_most_once_per_dispatch() {
        assert!(!should_recover_service_error(
            &mcp_service_err(-32603),
            true,
            true,
        ));
    }

    #[test]
    fn should_recover_service_error_http_mcperror_auth_rejection_not_recovered() {
        let auth_err = ServiceError::McpError(rmcp::ErrorData::new(
            rmcp::model::ErrorCode(-32603),
            "Unauthorized: token expired",
            None,
        ));
        assert!(!should_recover_service_error(&auth_err, true, false));
        let session_err = ServiceError::McpError(rmcp::ErrorData::new(
            rmcp::model::ErrorCode(-32603),
            "session not found",
            None,
        ));
        assert!(should_recover_service_error(&session_err, true, false));
    }

    #[test]
    fn should_recover_service_error_transport_errors_always_recover() {
        assert!(should_recover_service_error(
            &ServiceError::TransportClosed,
            true,
            false
        ));
        assert!(should_recover_service_error(
            &ServiceError::TransportClosed,
            false,
            false
        ));
        assert!(should_recover_service_error(
            &ServiceError::TransportClosed,
            true,
            true
        ));
    }

    #[test]
    fn should_recover_service_error_other_non_transport_not_recovered() {
        assert!(!should_recover_service_error(
            &ServiceError::UnexpectedResponse,
            true,
            false
        ));
        assert!(!should_recover_service_error(
            &ServiceError::Timeout {
                timeout: std::time::Duration::from_secs(30),
            },
            true,
            false
        ));
    }

    #[tokio::test]
    async fn recover_and_retry_surfaces_original_error_when_recover_fails() {
        let config = HttpConfig {
            url: "http://192.0.2.1:1/unreachable".to_string(),
            headers: vec![],
        };
        let overrides = McpClientTimeoutOverrides {
            startup_timeout_sec: Some(1),
            ..Default::default()
        };
        let client = Arc::new(McpClient::new_http(
            "wedged".to_string(),
            config,
            Some(&overrides),
            None,
        ));

        let tool = McpErasedTool {
            tool: McpTool::new(
                "do_thing".to_string(),
                "desc".to_string(),
                "wedged".to_string(),
                Arc::new(Mutex::new(McpState::new(vec![]))),
                serde_json::json!({"type": "object"}),
                None,
            ),
        };

        let original = mcp_service_err(-32603);
        let expected = original.to_string();
        let params = CallToolRequestParams::new("do_thing");

        let mut reconnect_attempted = false;
        let mut is_timeout = false;
        let ew = xai_file_utils::events::EventWriter::noop();

        let err = tool
            .recover_and_retry(
                &client,
                params,
                std::time::Duration::from_secs(1),
                1,
                original,
                &mut reconnect_attempted,
                &mut is_timeout,
                &ew,
            )
            .await
            .expect_err("recover must fail against an unreachable host");

        assert_eq!(err.to_string(), expected, "original error must be surfaced");
        assert!(reconnect_attempted, "reconnect attempt must be flagged");
        assert!(!is_timeout, "a recover failure is not a tool timeout");
    }

    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Copy)]
    enum CallToolBehavior {
        ErrorThenOk { code: i32 },
        AlwaysError { code: i32 },
        HangThenOk { hang_ms: u64 },
        ErrorThenHang { code: i32, hang_ms: u64 },
    }

    #[derive(Clone)]
    struct FakeMcpState {
        behavior: CallToolBehavior,
        inits: Arc<AtomicUsize>,
        calls: Arc<AtomicUsize>,
    }

    async fn fake_handle_post(
        axum::extract::State(state): axum::extract::State<FakeMcpState>,
        axum::Json(req): axum::Json<serde_json::Value>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        let id = req["id"].clone();
        let ok = || {
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id.clone(),
                "result": {"content": [{"type": "text", "text": "ok"}], "isError": false},
            })
        };
        let err = |code: i32, msg: String| {
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id.clone(),
                "error": {"code": code, "message": msg},
            })
        };
        match req["method"].as_str() {
            Some("initialize") => {
                state.inits.fetch_add(1, Ordering::Relaxed);
                let result = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id.clone(),
                    "result": {
                        "protocolVersion": req["params"]["protocolVersion"].clone(),
                        "capabilities": {},
                        "serverInfo": {"name": "fake", "version": "0.0.0"},
                    },
                });
                ([("mcp-session-id", "fake-session")], axum::Json(result)).into_response()
            }
            Some("tools/list") => axum::Json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id.clone(),
                "result": {"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]},
            }))
            .into_response(),
            Some("tools/call") => {
                let n = state.calls.fetch_add(1, Ordering::Relaxed);
                match state.behavior {
                    CallToolBehavior::ErrorThenOk { code } => {
                        if n == 0 {
                            axum::Json(err(code, "session expired".to_string())).into_response()
                        } else {
                            axum::Json(ok()).into_response()
                        }
                    }
                    CallToolBehavior::AlwaysError { code } => {
                        axum::Json(err(code, format!("attempt {}", n + 1))).into_response()
                    }
                    CallToolBehavior::HangThenOk { hang_ms } => {
                        if n == 0 {
                            tokio::time::sleep(std::time::Duration::from_millis(hang_ms)).await;
                        }
                        axum::Json(ok()).into_response()
                    }
                    CallToolBehavior::ErrorThenHang { code, hang_ms } => {
                        if n == 0 {
                            axum::Json(err(code, "session expired".to_string())).into_response()
                        } else {
                            tokio::time::sleep(std::time::Duration::from_millis(hang_ms)).await;
                            axum::Json(ok()).into_response()
                        }
                    }
                }
            }
            _ => axum::http::StatusCode::ACCEPTED.into_response(),
        }
    }

    async fn fake_handle_get() -> axum::response::Response {
        use axum::response::IntoResponse;
        let body = axum::body::Body::from_stream(futures::stream::pending::<
            Result<String, std::io::Error>,
        >());
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            body,
        )
            .into_response()
    }

    async fn spawn_fake_mcp(
        behavior: CallToolBehavior,
    ) -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let inits = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new()
            .route(
                "/mcp",
                axum::routing::get(fake_handle_get).post(fake_handle_post),
            )
            .with_state(FakeMcpState {
                behavior,
                inits: inits.clone(),
                calls: calls.clone(),
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}/mcp"), inits, calls)
    }

    fn fake_http_client(url: &str, tool_timeout_sec: u64) -> Arc<McpClient> {
        let overrides = McpClientTimeoutOverrides {
            startup_timeout_sec: Some(5),
            tool_timeout_sec: Some(tool_timeout_sec),
            ..Default::default()
        };
        Arc::new(McpClient::new_http(
            "fake".to_string(),
            HttpConfig {
                url: url.to_string(),
                headers: vec![],
            },
            Some(&overrides),
            None,
        ))
    }

    fn fake_echo_tool() -> McpErasedTool {
        McpErasedTool {
            tool: McpTool::new(
                "echo".to_string(),
                "echo desc".to_string(),
                "fake".to_string(),
                Arc::new(Mutex::new(McpState::new(vec![]))),
                serde_json::json!({"type": "object"}),
                None,
            ),
        }
    }

    fn event_types(jsonl: &str) -> Vec<serde_json::Value> {
        jsonl
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn try_call_tool_http_mcperror_recovers_then_retry_succeeds() {
        let (url, inits, calls) =
            spawn_fake_mcp(CallToolBehavior::ErrorThenOk { code: -32603 }).await;
        let client = fake_http_client(&url, 5);
        let tool = fake_echo_tool();
        let tmp = tempfile::tempdir().unwrap();
        let ew = xai_file_utils::events::EventWriter::open(tmp.path());

        let mut reconnect = false;
        let mut is_timeout = false;
        let raw = serde_json::json!({});
        let out = tool
            .try_call_tool(&client, &raw, &mut reconnect, &mut is_timeout, &ew)
            .await
            .expect("recovered call should succeed");

        assert!(
            !out.is_error.unwrap_or(false),
            "retry should return a success result"
        );
        assert!(reconnect, "reconnect_attempted must be set");
        assert!(!is_timeout);
        assert_eq!(
            calls.load(Ordering::Relaxed),
            2,
            "one failed + one retried tools/call"
        );
        assert_eq!(
            inits.load(Ordering::Relaxed),
            2,
            "initial handshake + one recovery re-init"
        );

        let jsonl = std::fs::read_to_string(tmp.path().join("events.jsonl")).unwrap();
        let events = event_types(&jsonl);
        assert!(
            events.iter().any(|e| e["type"] == "mcp_transport_error"),
            "expected mcp_transport_error in {jsonl}"
        );
        assert!(
            events
                .iter()
                .any(|e| e["type"] == "mcp_transport_reconnect" && e["success"] == true),
            "expected a successful mcp_transport_reconnect in {jsonl}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn try_call_tool_http_retry_failure_surfaces_retry_error() {
        let (url, _inits, calls) =
            spawn_fake_mcp(CallToolBehavior::AlwaysError { code: -32603 }).await;
        let client = fake_http_client(&url, 5);
        let tool = fake_echo_tool();
        let ew = xai_file_utils::events::EventWriter::noop();

        let mut reconnect = false;
        let mut is_timeout = false;
        let raw = serde_json::json!({});
        let err = tool
            .try_call_tool(&client, &raw, &mut reconnect, &mut is_timeout, &ew)
            .await
            .expect_err("both attempts fail");

        let msg = err.to_string();
        assert!(msg.contains("attempt 2"), "want retry error, got: {msg}");
        assert!(
            !msg.contains("attempt 1"),
            "must not surface the original error: {msg}"
        );
        assert!(reconnect);
        assert!(!is_timeout);
        assert_eq!(
            calls.load(Ordering::Relaxed),
            2,
            "one failed + one retried tools/call"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn try_call_tool_http_invalid_params_not_recovered() {
        let (url, inits, calls) =
            spawn_fake_mcp(CallToolBehavior::AlwaysError { code: -32602 }).await;
        let client = fake_http_client(&url, 5);
        let tool = fake_echo_tool();
        let ew = xai_file_utils::events::EventWriter::noop();

        let mut reconnect = false;
        let mut is_timeout = false;
        let raw = serde_json::json!({});
        let err = tool
            .try_call_tool(&client, &raw, &mut reconnect, &mut is_timeout, &ew)
            .await
            .expect_err("invalid params surfaced as-is");

        assert!(err.to_string().contains("attempt 1"), "got: {err}");
        assert!(!reconnect, "invalid-params must not trigger recovery");
        assert!(!is_timeout);
        assert_eq!(calls.load(Ordering::Relaxed), 1, "no retry POST");
        assert_eq!(inits.load(Ordering::Relaxed), 1, "no recovery re-init");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn try_call_tool_http_outer_timeout_resets_transport_no_retry() {
        let (url, inits, calls) =
            spawn_fake_mcp(CallToolBehavior::HangThenOk { hang_ms: 3000 }).await;
        let client = fake_http_client(&url, 1);
        let tool = fake_echo_tool();
        let ew = xai_file_utils::events::EventWriter::noop();

        let mut reconnect = false;
        let mut is_timeout = false;
        let raw = serde_json::json!({});
        let err = tool
            .try_call_tool(&client, &raw, &mut reconnect, &mut is_timeout, &ew)
            .await
            .expect_err("call must time out");

        assert!(err.to_string().contains("timed out"), "got: {err}");
        assert!(is_timeout, "is_timeout must be set");
        assert!(reconnect, "timeout arm flags the reconnect after resetting");
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "timed-out call is NOT retried"
        );
        assert!(matches!(
            client.state_kind().await,
            ClientStateKind::Pending
        ));
        assert_eq!(
            inits.load(Ordering::Relaxed),
            1,
            "no re-init during the timed-out dispatch"
        );

        let mut reconnect2 = false;
        let mut is_timeout2 = false;
        let out = tool
            .try_call_tool(&client, &raw, &mut reconnect2, &mut is_timeout2, &ew)
            .await
            .expect("second dispatch should re-init and succeed");
        assert!(!out.is_error.unwrap_or(false));
        assert!(!is_timeout2);
        assert_eq!(
            inits.load(Ordering::Relaxed),
            2,
            "second dispatch re-initialized the session"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn try_call_tool_http_retry_timeout_surfaces_timeout() {
        let (url, inits, calls) = spawn_fake_mcp(CallToolBehavior::ErrorThenHang {
            code: -32603,
            hang_ms: 3000,
        })
        .await;
        let client = fake_http_client(&url, 1);
        let tool = fake_echo_tool();
        let ew = xai_file_utils::events::EventWriter::noop();

        let mut reconnect = false;
        let mut is_timeout = false;
        let raw = serde_json::json!({});
        let err = tool
            .try_call_tool(&client, &raw, &mut reconnect, &mut is_timeout, &ew)
            .await
            .expect_err("the retried call must time out");

        assert!(err.to_string().contains("timed out"), "got: {err}");
        assert!(is_timeout, "retry-timeout must set is_timeout");
        assert!(reconnect, "recovery was attempted");
        assert_eq!(
            calls.load(Ordering::Relaxed),
            2,
            "the retry tools/call was attempted"
        );
        assert_eq!(
            inits.load(Ordering::Relaxed),
            2,
            "recovery re-initialized before the retry"
        );
    }

    // ── new_http stores http_config tests ────────────────────────────

    #[test]
    fn test_new_http_stores_http_config() {
        let config = HttpConfig {
            url: "http://localhost:5000/api/mcp".to_string(),
            headers: vec![("x-token".to_string(), "abc".to_string())],
        };
        let client = McpClient::new_http("example-mcp".to_string(), config, None, None);
        let stored = client
            .http_config
            .as_ref()
            .expect("http_config should be Some");
        assert_eq!(stored.url, "http://localhost:5000/api/mcp");
        assert_eq!(stored.headers.len(), 1);
        assert_eq!(stored.headers[0].0, "x-token");
    }

    #[test]
    fn test_new_stdio_has_no_http_config() {
        // Stdio clients must NOT have http_config — they can't reconnect via HTTP.
        let client = McpClient::stub("stdio-srv");
        assert!(client.http_config.is_none());
    }

    // ── http_headers_match / refresh_managed_clients guard tests ─────

    #[test]
    fn http_headers_match_compares_full_set_order_insensitively() {
        let config = HttpConfig {
            url: "http://localhost:5000/api/mcp".to_string(),
            headers: vec![
                ("authorization".to_string(), "Bearer t".to_string()),
                ("x-scope".to_string(), "read".to_string()),
            ],
        };
        let client = McpClient::new_http("managed".to_string(), config, None, None);

        let equal: HashMap<String, String> = [
            ("x-scope".to_string(), "read".to_string()),
            ("authorization".to_string(), "Bearer t".to_string()),
        ]
        .into_iter()
        .collect();
        assert!(client.http_headers_match(&equal));

        let changed_value: HashMap<String, String> = [
            ("authorization".to_string(), "Bearer NEW".to_string()),
            ("x-scope".to_string(), "read".to_string()),
        ]
        .into_iter()
        .collect();
        assert!(!client.http_headers_match(&changed_value));

        let missing_key: HashMap<String, String> =
            [("authorization".to_string(), "Bearer t".to_string())]
                .into_iter()
                .collect();
        assert!(!client.http_headers_match(&missing_key));
    }

    #[test]
    fn http_headers_match_handles_duplicate_stored_keys() {
        // Duplicate stored key must not mask a missing fresh key by inflating
        // the stored length to match.
        let config = HttpConfig {
            url: "http://localhost:5000/api/mcp".to_string(),
            headers: vec![
                ("authorization".to_string(), "Bearer t".to_string()),
                ("authorization".to_string(), "Bearer t".to_string()),
            ],
        };
        let client = McpClient::new_http("managed".to_string(), config, None, None);

        let two_distinct: HashMap<String, String> = [
            ("authorization".to_string(), "Bearer t".to_string()),
            ("x-scope".to_string(), "read".to_string()),
        ]
        .into_iter()
        .collect();
        assert!(!client.http_headers_match(&two_distinct));

        let single: HashMap<String, String> =
            [("authorization".to_string(), "Bearer t".to_string())]
                .into_iter()
                .collect();
        assert!(client.http_headers_match(&single));
    }

    #[test]
    fn http_headers_match_false_for_non_http_client() {
        let client = McpClient::stub("stdio-srv");
        let headers: HashMap<String, String> =
            [("authorization".to_string(), "Bearer t".to_string())]
                .into_iter()
                .collect();
        assert!(!client.http_headers_match(&headers));
    }

    #[test]
    fn refresh_managed_clients_keeps_arc_when_headers_unchanged() {
        let url = "http://localhost:5000/api/mcp";
        let mut state = McpState::new(vec![make_http_server("managed", url)]);
        let config = HttpConfig {
            url: url.to_string(),
            headers: vec![("authorization".to_string(), "Bearer t".to_string())],
        };
        state.owned_clients.insert(
            "managed".to_string(),
            Arc::new(McpClient::new_http(
                "managed".to_string(),
                config,
                None,
                None,
            )),
        );
        let before = Arc::clone(state.owned_clients.get("managed").unwrap());

        let fresh: HashMap<String, String> =
            [("authorization".to_string(), "Bearer t".to_string())]
                .into_iter()
                .collect();
        state.refresh_managed_clients(std::iter::once((url, &fresh)));

        let after = state.owned_clients.get("managed").unwrap();
        assert!(
            Arc::ptr_eq(&before, after),
            "unchanged headers must not rebuild the client"
        );
    }

    #[test]
    fn refresh_managed_clients_installs_new_arc_when_headers_differ() {
        let url = "http://localhost:5000/api/mcp";
        let mut state = McpState::new(vec![make_http_server("managed", url)]);
        let config = HttpConfig {
            url: url.to_string(),
            headers: vec![("authorization".to_string(), "Bearer old".to_string())],
        };
        state.owned_clients.insert(
            "managed".to_string(),
            Arc::new(McpClient::new_http(
                "managed".to_string(),
                config,
                None,
                None,
            )),
        );
        let before = Arc::clone(state.owned_clients.get("managed").unwrap());

        let fresh: HashMap<String, String> =
            [("authorization".to_string(), "Bearer new".to_string())]
                .into_iter()
                .collect();
        state.refresh_managed_clients(std::iter::once((url, &fresh)));

        let after = state.owned_clients.get("managed").unwrap();
        assert!(
            !Arc::ptr_eq(&before, after),
            "changed headers must install a fresh client"
        );
        assert!(after.http_headers_match(&fresh));
    }

    // ── reset_transport tests ────────────────────────────────────────

    #[tokio::test]
    async fn test_reset_transport_succeeds_for_http_client() {
        let config = HttpConfig {
            url: "http://127.0.0.1:9/api/mcp".to_string(),
            headers: vec![],
        };
        let client = McpClient::new_http("example-mcp".to_string(), config, None, None);
        assert!(client.reset_transport().await);
    }

    #[tokio::test]
    async fn test_reset_transport_fails_for_stub() {
        // Stub has `reconnect = None`, simulating a Stdio client.
        let client = McpClient::stub("stdio-srv");
        assert!(!client.reset_transport().await);
    }

    #[tokio::test]
    async fn test_reset_transport_is_idempotent() {
        let config = HttpConfig {
            url: "http://127.0.0.1:9/api/mcp".to_string(),
            headers: vec![],
        };
        let client = McpClient::new_http("example-mcp".to_string(), config, None, None);

        // Multiple resets should all succeed.
        assert!(client.reset_transport().await);
        assert!(client.reset_transport().await);
        assert!(client.reset_transport().await);
    }

    #[tokio::test]
    async fn test_reset_transport_makes_ensure_initialized_retry_handshake() {
        // Port 1 on loopback refuses immediately (ECONNREFUSED -> HandshakeFailed),
        // so each handshake fails fast instead of waiting out the connect timeout.
        let config = HttpConfig {
            url: "http://127.0.0.1:1/unreachable".to_string(),
            headers: vec![],
        };
        let client = McpClient::new_http("test".to_string(), config, None, None);

        // First ensure_initialized will fail (unreachable server) but proves
        // the client attempts a handshake from the Pending state.
        let err1 = client.ensure_initialized().await.unwrap_err();
        assert!(
            matches!(
                err1,
                McpError::Timeout { .. } | McpError::HandshakeFailed { .. }
            ),
            "first init should fail: {err1}"
        );

        // Reset puts the client back into Pending with a fresh transport.
        assert!(client.reset_transport().await);

        // Second ensure_initialized should attempt another handshake (not
        // return a cached error). It will fail again with the same kind of
        // error, proving the reset restored the transport.
        let err2 = client.ensure_initialized().await.unwrap_err();
        assert!(
            matches!(
                err2,
                McpError::Timeout { .. } | McpError::HandshakeFailed { .. }
            ),
            "second init after reset should also attempt handshake: {err2}"
        );
    }

    #[tokio::test]
    async fn recover_errors_for_client_with_no_restorable_transport() {
        // A stub has `reconnect = None` (like Stdio): `recover` can't rebuild it.
        let err = Arc::new(McpClient::stub("stdio"))
            .recover()
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::ClientError(_)), "got {err}");
    }

    #[tokio::test]
    async fn reset_transport_rebuilds_acp_client() {
        use crate::acp_transport::AcpReverseInvoker;
        use std::time::Duration;

        struct NoopInvoker;
        #[async_trait::async_trait]
        impl AcpReverseInvoker for NoopInvoker {
            async fn invoke(
                &self,
                _server_id: &str,
                _message: serde_json::Value,
                _timeout: Duration,
            ) -> Result<serde_json::Value, String> {
                Ok(serde_json::Value::Null)
            }
        }

        let client = McpClient::new_acp(
            "sdk-tools".to_string(),
            "srv_0".to_string(),
            Arc::new(NoopInvoker),
            None,
            None,
        );

        // ACP clients restore from `reconnect`, unlike Stdio.
        assert!(client.reset_transport().await);
        assert!(
            matches!(
                &*client.state.lock().await,
                ClientState::Pending(PendingTransport::Acp { .. })
            ),
            "reset_transport should restore the ACP transport to Pending"
        );
    }

    /// End-to-end reconnect-THEN-SUCCEED for the `try_call_tool` retry arm: the one
    /// piece otherwise covered only by its parts (`is_retriable_transport_error`,
    /// `reset_transport_*`, `ensure_initialized_*`).
    ///
    /// Drives the REAL `McpErasedTool::try_call_tool` against a real
    /// `McpClient`. The first `call_tool` hits a real `RunningService`
    /// whose transport is already closed, so it returns a genuine,
    /// retriable `ServiceError::TransportClosed`; the arm must then flag
    /// `reconnect_attempted`, run the real `reset_transport` +
    /// `ensure_initialized` re-handshake (rebuilding the ACP transport
    /// against a working echo server), and return the SECOND attempt's
    /// `Ok` result.
    ///
    /// Why a separately-built dead service instead of failing the initial
    /// connection: the ACP bridge transport can only be torn down from the
    /// rmcp side, so a fresh real service is built over a raw duplex whose
    /// server answers `initialize` then drops — closing the transport so
    /// the first `call_tool` observes `TransportClosed`. Everything from
    /// the retriable-error gate through the successful retry is real code.
    #[tokio::test]
    async fn try_call_tool_reconnects_then_succeeds_after_retriable_transport_error() {
        use crate::acp_transport::AcpReverseInvoker;
        use std::time::Duration;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        // Working in-process echo server for the post-reconnect retry.
        struct EchoSdkServer;
        #[async_trait::async_trait]
        impl AcpReverseInvoker for EchoSdkServer {
            async fn invoke(
                &self,
                _server_id: &str,
                message: serde_json::Value,
                _timeout: Duration,
            ) -> Result<serde_json::Value, String> {
                let id = message
                    .get("id")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let method = message
                    .get("method")
                    .and_then(|m| m.as_str())
                    .unwrap_or_default();
                let result = match method {
                    "initialize" => serde_json::json!({
                        "protocolVersion": message["params"]["protocolVersion"],
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "echo", "version": "0.0.0" },
                    }),
                    "tools/call" => serde_json::json!({
                        "content": [{
                            "type": "text",
                            "text": message["params"]["arguments"]["text"]
                                .as_str()
                                .unwrap_or_default(),
                        }],
                        "isError": false,
                    }),
                    other => return Err(format!("unexpected method {other}")),
                };
                Ok(serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }))
            }
        }

        // A real `RunningService` whose transport is already closed: the
        // server answers `initialize`, consumes the `initialized`
        // notification (so the client's handshake send succeeds), then drops
        // its duplex ends. The next `call_tool` therefore observes a real
        // `ServiceError::TransportClosed`.
        async fn dead_service() -> McpService {
            let (client_read, server_write) = tokio::io::duplex(64 * 1024); // server -> client
            let (server_read, client_write) = tokio::io::duplex(64 * 1024); // client -> server
            tokio::spawn(async move {
                let mut reader = BufReader::new(server_read);
                let mut writer = server_write;
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let Ok(msg) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                        continue;
                    };
                    if msg.get("method").and_then(|m| m.as_str()) == Some("initialize") {
                        let id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
                        let resp = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": {
                            "protocolVersion": msg["params"]["protocolVersion"],
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "dead", "version": "0.0.0" },
                        }});
                        let mut encoded = serde_json::to_string(&resp).unwrap();
                        encoded.push('\n');
                        let _ = writer.write_all(encoded.as_bytes()).await;
                        let _ = writer.flush().await;
                        // Drain the `initialized` notification, then drop to close.
                        let _ = reader.read_line(&mut line).await;
                        return;
                    }
                }
            });
            let handler = GrokClientHandler {
                info: McpClient::make_client_info("dead"),
                server_name: "dead".to_string(),
                notify_tx: Arc::new(parking_lot::Mutex::new(None)),
            };
            let transport = rmcp::transport::async_rw::AsyncRwTransport::<RoleClient, _, _>::new(
                client_read,
                client_write,
            );
            Arc::new(
                handler
                    .serve(transport)
                    .await
                    .expect("dead-service handshake"),
            )
        }

        // ACP client whose `reconnect` snapshot rebuilds against the echo server.
        let client = Arc::new(McpClient::new_acp(
            "sdk".to_string(),
            "srv_0".to_string(),
            Arc::new(EchoSdkServer),
            None,
            None,
        ));
        // Inject the closed real service so the FIRST `call_tool` fails retriably.
        let dead = dead_service().await;
        *client.state.lock().await = ClientState::Ready(dead);

        let erased = McpErasedTool {
            tool: McpTool::new(
                "echo".to_string(),
                "echo".to_string(),
                "sdk".to_string(),
                Arc::new(Mutex::new(McpState::new(vec![]))),
                serde_json::json!({}),
                None,
            ),
        };

        let raw = serde_json::json!({ "text": "after reconnect" });
        let mut reconnect_attempted = false;
        let mut is_timeout = false;
        let ew = xai_file_utils::events::EventWriter::noop();
        let result = erased
            .try_call_tool(
                &client,
                &raw,
                &mut reconnect_attempted,
                &mut is_timeout,
                &ew,
            )
            .await
            .expect("retry after reconnect should succeed");

        // The Ok came from the SECOND attempt — the dead service cannot echo,
        // so this text proves the rebuilt transport served the retry.
        assert_eq!(
            result.content[0].as_text().expect("text content").text,
            "after reconnect"
        );
        assert!(
            reconnect_attempted,
            "retriable transport error must set reconnect_attempted"
        );
        assert!(
            !is_timeout,
            "successful retry must not be flagged as timeout"
        );
        // reset_transport + re-handshake replaced the dead service with a live one.
        assert!(matches!(&*client.state.lock().await, ClientState::Ready(_)));
    }

    #[test]
    fn is_auth_rejection_message_matches_auth_signals() {
        // The verbatim string captured in production for a managed handshake.
        assert!(is_auth_rejection_message(
            "MCP server 'grok_com_notion' handshake failed: Auth required, when send initialize request"
        ));
        assert!(is_auth_rejection_message("401 Unauthorized"));
        assert!(is_auth_rejection_message("unauthorized"));
        assert!(is_auth_rejection_message("Authentication required"));
        assert!(is_auth_rejection_message("authentication failed"));
        assert!(is_auth_rejection_message("status: 401"));
        assert!(is_auth_rejection_message("HTTP status 401"));
        assert!(is_auth_rejection_message("server returned status code 401"));
        assert!(is_auth_rejection_message("HTTP 401"));
        assert!(is_auth_rejection_message("error 401"));
        // rmcp worker fatal context uses Debug form without spaces.
        assert!(is_auth_rejection_message(
            "worker quit with fatal: Transport channel closed, when Auth(AuthorizationRequired)"
        ));
        let auth_req = McpError::AuthRequired {
            server: "clickhouse".into(),
        };
        assert!(auth_req.is_auth_rejection());
        assert_eq!(auth_req.server_name(), Some("clickhouse"));
    }

    #[test]
    fn auth_required_records_as_auth_not_init_failed_and_maps_category() {
        // Pre-spawn gate is owned by the auth state machine: it lands in
        // `auth_required` (recoverable via re-auth) and never `init_failed`.
        let mut state = McpState::new(vec![]);
        state.record_init_failure("oauth-srv", true, None);
        assert!(state.auth_required.contains("oauth-srv"));
        assert!(!state.init_failed.contains_key("oauth-srv"));

        // AuthRequired carries the AuthRequired telemetry category, not ClientError.
        let err = McpError::AuthRequired {
            server: "oauth-srv".into(),
        };
        assert!(matches!(
            err.error_category(),
            xai_file_utils::events::McpErrorCategory::AuthRequired
        ));
    }

    #[test]
    fn is_auth_rejection_message_rejects_non_auth() {
        // Transport / timeout / spawn wording is never an auth rejection.
        assert!(!is_auth_rejection_message("Transport closed"));
        assert!(!is_auth_rejection_message(
            "MCP server 'x' timed out after 30s"
        ));
        assert!(!is_auth_rejection_message(
            "Failed to spawn MCP server 'x': No such file or directory"
        ));
        // 403/forbidden is a non-auth policy denial in this stack, not auth.
        assert!(!is_auth_rejection_message("403 Forbidden"));
        assert!(!is_auth_rejection_message("forbidden"));
        // Incidental digits must not trip the status-anchored 401 patterns.
        assert!(!is_auth_rejection_message("request took 401ms"));
        assert!(!is_auth_rejection_message("connect 10.0.4.01:443"));
        assert!(!is_auth_rejection_message("read 401 bytes"));
        // A status literal followed by another alphanumeric is a different
        // token: a longer number (4012) or an adjacent unit (401ms).
        assert!(!is_auth_rejection_message("http 4012"));
        assert!(!is_auth_rejection_message("error 4012"));
        assert!(!is_auth_rejection_message("status: 4012"));
        assert!(!is_auth_rejection_message("http 401ms"));
        assert!(!is_auth_rejection_message("error 401ms"));
        // ...but a trailing punctuation/whitespace still matches.
        assert!(is_auth_rejection_message("http 401."));
        assert!(is_auth_rejection_message("error 401: token expired"));
    }

    #[test]
    fn mcp_error_is_auth_rejection_delegates() {
        assert!(McpError::ClientError("Auth required".to_string()).is_auth_rejection());
        assert!(!McpError::ClientError("Transport closed".to_string()).is_auth_rejection());
        assert!(
            !McpError::Timeout {
                server: "x".to_string(),
                timeout_secs: 30,
            }
            .is_auth_rejection()
        );
        assert!(
            !McpError::SpawnFailed {
                server: "x".to_string(),
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "401 Unauthorized"),
            }
            .is_auth_rejection()
        );
        // HandshakeFailed is the production carrier: its `source` Display must
        // surface the auth substring for the delegation to fire.
        assert!(
            McpError::HandshakeFailed {
                server: "x".to_string(),
                source: Box::new(ClientInitializeError::ConnectionClosed(
                    "Auth required, when send initialize request".to_string()
                )),
            }
            .is_auth_rejection()
        );
        assert!(
            !McpError::HandshakeFailed {
                server: "x".to_string(),
                source: Box::new(ClientInitializeError::ConnectionClosed(
                    "transport closed".to_string()
                )),
            }
            .is_auth_rejection()
        );
    }

    #[test]
    fn format_mcp_image_default_emits_only_data_uri() {
        let out = format_mcp_image("image/png", "AAAA", false);
        assert_eq!(out, "data:image/png;base64,AAAA");
        assert!(!out.contains("<mcp_image_base64"));
    }

    #[test]
    fn format_mcp_image_expose_emits_data_uri_and_raw_block() {
        let out = format_mcp_image("image/png", "AAAA", true);
        assert!(out.contains("data:image/png;base64,AAAA"));
        assert!(out.contains("<mcp_image_base64 mime=\"image/png\">\nAAAA\n</mcp_image_base64>"));
    }

    /// Wrapper must not re-match the extractor regex, else the raw copy gets stripped too.
    #[test]
    fn format_mcp_image_expose_raw_block_has_no_data_prefix() {
        let out = format_mcp_image("image/jpeg", "ZZZZ", true);
        assert_eq!(out.matches("data:image/").count(), 1);
    }

    #[test]
    fn load_expose_image_base64_defaults_to_false() {
        assert!(!McpClient::load_expose_image_base64(None, None));
    }

    #[test]
    fn load_expose_image_base64_uses_overrides_when_meta_unset() {
        let overrides = McpClientTimeoutOverrides {
            expose_image_base64: Some(true),
            ..Default::default()
        };
        assert!(McpClient::load_expose_image_base64(Some(&overrides), None));
    }

    #[test]
    fn load_expose_image_base64_meta_wins_over_overrides() {
        let overrides = McpClientTimeoutOverrides {
            expose_image_base64: Some(true),
            ..Default::default()
        };
        let meta = McpServerMetaConfig {
            expose_image_base64: Some(false),
            ..Default::default()
        };
        assert!(!McpClient::load_expose_image_base64(
            Some(&overrides),
            Some(&meta)
        ));
    }

    #[test]
    fn load_expose_image_base64_meta_falls_through_when_none() {
        let overrides = McpClientTimeoutOverrides {
            expose_image_base64: Some(true),
            ..Default::default()
        };
        let meta = McpServerMetaConfig::default(); // expose_image_base64 = None
        assert!(McpClient::load_expose_image_base64(
            Some(&overrides),
            Some(&meta)
        ));
    }

    /// End-to-end: override → constructor → public getter.
    /// New constructors should add a similar assertion.
    #[test]
    fn new_http_propagates_expose_image_base64_override_to_getter() {
        let config = HttpConfig {
            url: "http://localhost/api/mcp".to_string(),
            headers: vec![],
        };
        let overrides = McpClientTimeoutOverrides {
            expose_image_base64: Some(true),
            ..Default::default()
        };
        let client = McpClient::new_http(
            "grafana".to_string(),
            config.clone(),
            Some(&overrides),
            None,
        );
        assert!(client.expose_image_base64());

        let client_default = McpClient::new_http("grafana".to_string(), config, None, None);
        assert!(!client_default.expose_image_base64());
    }

    // ------------------------------------------------------------------
    // ensure_initialized single-flight + Notify behavior (regression
    // suite for the "MCP client already initializing" doom-loop).
    // ------------------------------------------------------------------

    /// `ensure_initialized` on a stub (no transport) must surface a
    /// clear, actionable configuration error — never the legacy
    /// "already initializing" sentinel which leaked into model-visible
    /// tool results and triggered retry loops that exhausted the
    /// per-tick prompt budget.
    #[tokio::test]
    async fn ensure_initialized_on_empty_client_returns_no_transport_error() {
        let client = McpClient::stub("test-server");

        let err = client.ensure_initialized().await.unwrap_err();
        let msg = err.to_string();

        assert!(
            msg.contains("no transport configured"),
            "expected clear 'no transport configured' error, got: {msg}"
        );
        assert!(
            !msg.contains("already initializing"),
            "regression: legacy fast-fail sentinel surfaced: {msg}"
        );
    }

    /// Drive `N` `ensure_initialized` calls concurrently against an
    /// unreachable HTTP server with a tight startup timeout. Every
    /// caller must surface a real handshake error (`Timeout` or
    /// `HandshakeFailed`); none may surface the legacy
    /// "MCP client already initializing" sentinel which the
    /// pre-fix branch emitted whenever a caller observed
    /// `Pending(None)` while another caller was running the handshake.
    ///
    /// The race window is intentionally widened by using an unreachable
    /// host (`192.0.2.1:1` — TEST-NET-1, guaranteed unrouteable) so the
    /// handshake stalls for `startup_timeout_sec` and every concurrent
    /// caller spawned after the first observes `Initializing` instead
    /// of `Pending`.
    #[tokio::test]
    async fn ensure_initialized_concurrent_callers_never_see_legacy_fast_fail() {
        let config = HttpConfig {
            url: "http://192.0.2.1:1/unreachable".to_string(),
            headers: vec![],
        };
        let overrides = McpClientTimeoutOverrides {
            startup_timeout_sec: Some(1),
            ..Default::default()
        };
        let client = Arc::new(McpClient::new_http(
            "test-server".to_string(),
            config,
            Some(&overrides),
            None,
        ));

        let mut handles = Vec::new();
        for _ in 0..5 {
            let c = Arc::clone(&client);
            handles.push(tokio::spawn(async move { c.ensure_initialized().await }));
        }

        for (idx, handle) in handles.into_iter().enumerate() {
            let result = handle.await.expect("task did not panic");
            let err = result.expect_err("unreachable host must fail");
            let msg = err.to_string();
            assert!(
                !msg.contains("MCP client already initializing"),
                "caller {idx}: legacy fast-fail sentinel surfaced: {msg}"
            );
            assert!(
                matches!(
                    err,
                    McpError::Timeout { .. } | McpError::HandshakeFailed { .. }
                ),
                "caller {idx}: expected handshake failure, got: {err}"
            );
        }
    }

    /// A caller that finds `ClientState::Initializing` must park on
    /// `init_done` and wake up when the holder publishes a new state,
    /// then take the freshly-restored transport for its own retry.
    ///
    /// We exercise the wake path directly (without an actual concurrent
    /// handshake) by manually transitioning state to `Initializing`,
    /// spawning a parker, then transitioning back to `Pending` and
    /// firing `notify_waiters`. The parker should retry against the
    /// restored (still-unreachable) transport and surface a normal
    /// handshake error rather than the wait-timeout error.
    #[tokio::test]
    async fn ensure_initialized_parked_caller_retries_after_notify() {
        let config = HttpConfig {
            url: "http://192.0.2.1:1/unreachable".to_string(),
            headers: vec![],
        };
        let overrides = McpClientTimeoutOverrides {
            startup_timeout_sec: Some(1),
            ..Default::default()
        };
        let client = Arc::new(McpClient::new_http(
            "test-server".to_string(),
            config.clone(),
            Some(&overrides),
            None,
        ));

        // Simulate an in-flight handshake by another task: pretend
        // that task took the transport and entered Initializing.
        *client.state.lock().await = ClientState::Initializing;

        // Spawn the parker. It must observe Initializing and park on
        // `init_done` rather than fail-fast.
        let parker_client = Arc::clone(&client);
        let parker = tokio::spawn(async move { parker_client.ensure_initialized().await });

        // Give the parker a chance to reach the await on `init_done`.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Publish a fresh Pending transport and notify — simulates the
        // holder's failure-path restore.
        *client.state.lock().await = ClientState::Pending(PendingTransport::Http(config.clone()));
        client.init_done.notify_waiters();

        // The parker should wake, take the transport, run its own
        // handshake (which fails against the unreachable host), and
        // surface a regular handshake error — never the wait-timeout
        // error and never the legacy fast-fail.
        let err = parker
            .await
            .expect("parker did not panic")
            .expect_err("unreachable host must fail");
        let msg = err.to_string();
        assert!(
            !msg.contains("MCP client already initializing"),
            "regression: legacy fast-fail sentinel: {msg}"
        );
        assert!(
            !msg.contains("init still in progress"),
            "parker should not hit wait-timeout when notified: {msg}"
        );
        assert!(
            matches!(
                err,
                McpError::Timeout { .. } | McpError::HandshakeFailed { .. }
            ),
            "expected handshake failure, got: {err}"
        );
    }

    /// If a caller is parked on `Initializing` and the holder is
    /// dropped without notifying (cancellation-storm edge case), the
    /// parker must eventually surface a clear `init still in progress`
    /// timeout error rather than block indefinitely.
    ///
    /// Without the inflight-wait timeout, a wedged client (one whose
    /// drop guard couldn't acquire the lock to restore) would silently
    /// stall every future `ensure_initialized` caller until process
    /// restart. The 1 s margin past `startup_timeout_sec` keeps the
    /// happy path snappy while still bounding the worst case.
    #[tokio::test]
    async fn ensure_initialized_inflight_wait_times_out_when_holder_silent() {
        let config = HttpConfig {
            url: "http://192.0.2.1:1/unreachable".to_string(),
            headers: vec![],
        };
        let overrides = McpClientTimeoutOverrides {
            startup_timeout_sec: Some(0),
            ..Default::default()
        };
        let client = McpClient::new_http("test-server".to_string(), config, Some(&overrides), None);

        // Wedge the slot in Initializing with no live holder.
        *client.state.lock().await = ClientState::Initializing;

        let err = client.ensure_initialized().await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("init still in progress"),
            "expected wait-timeout error, got: {msg}"
        );
        assert!(
            !msg.contains("already initializing"),
            "regression: legacy fast-fail sentinel: {msg}"
        );
    }

    /// When the holder task is cancelled (`abort()`) mid-handshake, the
    /// `InitGuard` drop impl restores `Pending(transport)` on a
    /// best-effort basis so a follow-on caller can retry without
    /// requiring an explicit `reset_transport`.
    #[tokio::test]
    async fn ensure_initialized_drop_guard_restores_state_after_holder_aborted() {
        let config = HttpConfig {
            url: "http://192.0.2.1:1/unreachable".to_string(),
            headers: vec![],
        };
        let overrides = McpClientTimeoutOverrides {
            // Long enough that the holder is guaranteed to still be
            // inside try_handshake when we abort it.
            startup_timeout_sec: Some(10),
            ..Default::default()
        };
        let client = Arc::new(McpClient::new_http(
            "test-server".to_string(),
            config,
            Some(&overrides),
            None,
        ));

        let holder_client = Arc::clone(&client);
        let holder = tokio::spawn(async move { holder_client.ensure_initialized().await });

        // Wait for the holder to enter Initializing.
        let started = std::time::Instant::now();
        loop {
            if matches!(&*client.state.lock().await, ClientState::Initializing) {
                break;
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(2),
                "holder never reached Initializing"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // Cancel the holder mid-handshake. The drop guard should
        // restore Pending so the next caller can retry.
        holder.abort();
        let _ = holder.await;

        // The drop guard restores best-effort via `try_lock` and notifies.
        // Wait briefly for it to settle.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        match &*client.state.lock().await {
            ClientState::Pending(_) => {} // expected
            other => panic!(
                "expected Pending after holder abort + drop guard, found {}",
                state_label(other)
            ),
        }
    }

    /// `McpState::is_initialized()` MUST require both the early
    /// `finish_init` flag AND an empty `initializing_servers` set.
    ///
    /// The session actor's `start_mcp_servers` path calls `finish_init`
    /// **early** (right after spawning processes, before any handshake
    /// completes) so non-MCP work can proceed in parallel. Tool dispatch
    /// and the Blocking-strategy prompt guard, however, must NOT
    /// observe "initialized" until every per-server handshake is done —
    /// otherwise the model's first tool call races the background
    /// `get_tool_registrations` handshake and the
    /// `McpClient::ensure_initialized` window described above triggers.
    #[test]
    fn test_mcp_state_is_initialized_requires_empty_initializing_servers() {
        let mut state = McpState::new(vec![make_stdio_server("a", "/bin/a")]);

        // NotStarted: neither flag set, no per-server work.
        assert!(!state.is_initialized());
        assert!(!state.is_initializing());
        assert!(!state.has_finished_init());
        assert!(matches!(state.init_progress(), InitProgress::NotStarted));

        // Starting: try_start_init fired, per-server names registered,
        // finish_init has NOT yet fired. is_initializing() is true.
        assert!(state.try_start_init());
        state.mark_servers_initializing(["a".to_string()]);
        assert!(!state.is_initialized());
        assert!(state.is_initializing());
        assert!(!state.has_finished_init());
        assert!(matches!(
            state.init_progress(),
            InitProgress::Starting { .. }
        ));

        // Finished + handshakes outstanding: actor called finish_init
        // early but the per-server background handshake is still in
        // flight. is_initialized() must be FALSE during this window.
        state.finish_init();
        assert!(
            !state.is_initialized(),
            "is_initialized() must wait for per-server handshakes"
        );
        assert!(
            state.is_initializing(),
            "is_initializing() must report in-flight per-server work"
        );
        assert!(state.has_finished_init());
        assert!(state.is_server_handshaking("a"));
        assert_eq!(state.handshaking_servers_count(), 1);

        // Finished + empty: background task has reported the handshake
        // complete. Now and only now is the pool fully initialized.
        state.mark_server_ready("a");
        assert!(state.is_initialized());
        assert!(!state.is_initializing());
        assert!(state.has_finished_init());
        assert!(!state.is_server_handshaking("a"));
        assert_eq!(state.handshaking_servers_count(), 0);
    }

    /// Locks in the typed-state contract: the `init_progress` field
    /// makes nonsensical combinations like "initialized AND
    /// initializing" structurally unrepresentable. Every legal state
    /// has exactly one [`InitProgress`] variant; every transition is
    /// driven through the typed methods.
    #[test]
    fn test_init_progress_state_machine_invariants() {
        let mut state = McpState::new(vec![make_stdio_server("a", "/bin/a")]);

        // Invariant: try_start_init is one-shot per cycle.
        assert!(state.try_start_init());
        assert!(!state.try_start_init(), "double try_start_init is rejected");

        // Invariant: mark_all_servers_ready clears handshaking in
        // both Starting and Finished states; never resurrects them.
        state.mark_servers_initializing(["a".to_string(), "b".to_string()]);
        assert_eq!(state.handshaking_servers_count(), 2);
        state.mark_all_servers_ready();
        assert_eq!(state.handshaking_servers_count(), 0);
        assert!(
            matches!(state.init_progress(), InitProgress::Starting { .. }),
            "mark_all_servers_ready preserves the lifecycle variant"
        );

        // Invariant: finish_init from Starting → Finished preserves
        // (or in this case, the now-empty) handshaking set.
        state.finish_init();
        assert!(state.is_initialized());
        assert!(matches!(
            state.init_progress(),
            InitProgress::Finished { .. }
        ));

        // Invariant: cancel_init returns us cleanly to NotStarted,
        // ready for a new try_start_init.
        state.cancel_init();
        assert!(matches!(state.init_progress(), InitProgress::NotStarted));
        assert!(state.try_start_init(), "cancel_init re-enables init");
    }

    fn state_label(s: &ClientState) -> &'static str {
        match s {
            ClientState::Empty => "Empty",
            ClientState::Pending(_) => "Pending",
            ClientState::Initializing => "Initializing",
            ClientState::Ready(_) => "Ready",
        }
    }

    // -- is_healthy / state_kind --------------------------------------
    //
    // These tests cover the cheap, non-blocking predicate. They focus
    // on the state-machine inspection: any
    // non-`Ready` variant returns `false` for `is_healthy`, and
    // `state_kind` projects every variant onto the matching
    // [`ClientStateKind`].
    //
    // The two `Ready` cases
    // (`is_healthy_ready_open_returns_true` and
    // `is_healthy_transport_closed_returns_false`) require a real
    // `RunningService<RoleClient, InitializeRequestParams>`, which can
    // only be constructed through rmcp's `serve_client` path. That
    // path needs a peer that responds to the MCP initialize
    // handshake, and this crate intentionally does NOT enable rmcp's
    // `server` feature (see `Cargo.toml`). Wiring up a hand-rolled
    // JSON-RPC responder over `tokio::io::duplex` would balloon the
    // test scaffolding far beyond what these tests need. We therefore
    // exercise the `Ready` arm indirectly: the cheap predicate is a
    // single `match` on the state mutex plus
    // `Peer::is_transport_closed`, which is upstream-tested in rmcp
    // itself (`rmcp-2.1.0/tests/test_close_connection.rs`).

    #[tokio::test]
    async fn is_healthy_empty_returns_false() {
        let client = McpClient::stub("empty");
        // `stub` starts in `ClientState::Empty`.
        assert!(matches!(*client.state.lock().await, ClientState::Empty));
        assert!(!client.is_healthy().await);
        assert_eq!(client.state_kind().await, ClientStateKind::Empty);
    }

    #[tokio::test]
    async fn is_healthy_pending_returns_false() {
        let config = HttpConfig {
            url: "http://192.0.2.1:1/unreachable".to_string(),
            headers: vec![],
        };
        let client = McpClient::new_http("pending".to_string(), config, None, None);
        // `new_http` constructs with `ClientState::Pending(_)`.
        assert!(matches!(
            *client.state.lock().await,
            ClientState::Pending(_)
        ));
        assert!(!client.is_healthy().await);
        assert_eq!(client.state_kind().await, ClientStateKind::Pending);
    }

    #[tokio::test]
    async fn is_healthy_initializing_returns_false() {
        let client = McpClient::stub("initializing");
        *client.state.lock().await = ClientState::Initializing;
        assert!(!client.is_healthy().await);
        assert_eq!(client.state_kind().await, ClientStateKind::Initializing);
    }

    /// `is_healthy` MUST NOT trigger a handshake. Regression guard:
    /// the previous implementation called `ensure_initialized`, which
    /// for a `Pending` HTTP client pointing at an unreachable host
    /// would block for `startup_timeout_sec` seconds. The cheap
    /// predicate must return immediately.
    #[tokio::test]
    async fn is_healthy_pending_does_not_block_on_handshake() {
        let config = HttpConfig {
            url: "http://192.0.2.1:1/unreachable".to_string(),
            headers: vec![],
        };
        // Force a generous startup timeout — if the predicate
        // regressed to going through ensure_initialized, this test
        // would hang for ~10 s. We assert it completes in well under
        // a second.
        let overrides = McpClientTimeoutOverrides {
            startup_timeout_sec: Some(10),
            ..Default::default()
        };
        let client = McpClient::new_http(
            "pending-unreachable".to_string(),
            config,
            Some(&overrides),
            None,
        );
        let start = std::time::Instant::now();
        let healthy = client.is_healthy().await;
        let elapsed = start.elapsed();
        assert!(!healthy);
        // 1 s bound: the cheap path is microseconds, so this is a 10×
        // safety margin against cold-runtime / contended-CI jitter while
        // still firing well inside the 10 s blocking window that a
        // regressed predicate (back through `ensure_initialized`) would
        // sit in.
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "is_healthy must be a cheap state inspection, took {elapsed:?}"
        );
    }

    // -- GrokClientHandler --------------------------------------
    //
    // The handler's notification routing is the only behavior worth
    // unit-testing here; `get_info` is a literal `info.clone()` and
    // doesn't merit a test. `NotificationContext` is non-trivial to
    // construct outside of an rmcp `RunningService`, so we exercise
    // the routing through the `emit` helper that the trait methods
    // call. If the trait wiring (one-line `async move { self.emit(...) }`)
    // ever regresses, the integration tests against a real MCP
    // server will catch it.

    #[tokio::test]
    async fn client_handler_routes_tools_changed() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<McpClientEvent>();
        let handler = GrokClientHandler {
            info: McpClient::make_client_info("test"),
            server_name: "test".to_string(),
            notify_tx: Arc::new(parking_lot::Mutex::new(Some(tx))),
        };
        handler.emit(McpClientEvent::ToolsChanged {
            server: handler.server_name.clone(),
        });
        let ev = rx.recv().await.expect("event arrived");
        match ev {
            McpClientEvent::ToolsChanged { server } => assert_eq!(server, "test"),
            other => panic!("expected ToolsChanged, got {other:?}"),
        }
    }

    /// Contract: when `notify_tx` is `None` (subagent snapshot,
    /// no dispatcher), `emit` is a no-op and the trait methods
    /// must not panic.
    #[tokio::test]
    async fn client_handler_no_dispatcher_is_silent() {
        let handler = GrokClientHandler {
            info: McpClient::make_client_info("test"),
            server_name: "test".to_string(),
            notify_tx: Arc::new(parking_lot::Mutex::new(None)),
        };
        handler.emit(McpClientEvent::ToolsChanged {
            server: "test".to_string(),
        });
        // No assertion needed — reaching this line means no panic.
    }

    /// Contract: get_info returns a clone of the stored ClientInfo.
    #[tokio::test]
    async fn client_handler_get_info_round_trips() {
        let info = McpClient::make_client_info("test-srv");
        let handler = GrokClientHandler {
            info: info.clone(),
            server_name: "test-srv".to_string(),
            notify_tx: Arc::new(parking_lot::Mutex::new(None)),
        };
        let got = handler.get_info();
        // ClientInfo doesn't derive PartialEq; check the visible
        // fields the constructor sets.
        assert_eq!(got.client_info.name, info.client_info.name);
        assert_eq!(got.client_info.version, info.client_info.version);
    }

    // A sender wired *after* the handler is constructed must still
    // reach the live rmcp service loop. This test exercises the
    // post-construction wiring path: build a handler from a client
    // whose slot is `None`, then install a sender via
    // `client.set_event_tx` and verify the handler picks it up (the
    // handler holds a clone of the same shared Arc slot).
    #[tokio::test]
    async fn client_handler_observes_post_handshake_set_event_tx() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<McpClientEvent>();
        // McpClient::stub initializes notify_tx as `Arc<Mutex<None>>`.
        let client = Arc::new(McpClient::stub("test"));

        // Build the handler BEFORE wiring the sender — emulates
        // the production flow where `make_client_handler` is called
        // during `try_handshake` and the dispatcher is wired
        // separately.
        let handler = client.make_client_handler();

        // Confirm the slot is `None` at handler-construction time.
        assert!(handler.notify_tx.lock().is_none());

        // Now wire the sender on the client. Because the handler
        // holds a CLONE OF THE SAME ARC, this mutation is observed
        // by the handler's next `emit`.
        client.set_event_tx(Some(tx));

        handler.emit(McpClientEvent::ToolsChanged {
            server: "test".to_string(),
        });
        let ev = rx.recv().await.expect("event arrived");
        match ev {
            McpClientEvent::ToolsChanged { server } => assert_eq!(server, "test"),
            other => panic!("expected ToolsChanged, got {other:?}"),
        }
    }

    // Mirrors the post-construction wiring on the `ensure_initialized`
    // emit path: even though `Ready` / `HandshakeFailed` fire from
    // inside `try_handshake`, the slot is read at emit time through the
    // SAME shared Arc, so wiring `set_event_tx` BEFORE the handshake is
    // sufficient to capture these events.
    #[tokio::test]
    async fn event_tx_clone_observes_set_event_tx() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<McpClientEvent>();
        let client = McpClient::stub("test");
        assert!(client.event_tx_clone().is_none());
        client.set_event_tx(Some(tx));
        assert!(client.event_tx_clone().is_some());
        client.set_event_tx(None);
        assert!(client.event_tx_clone().is_none());
    }

    // An `ensure_initialized`-emitted `Ready` event must NOT be
    // conflated with a restart. This unit test exercises the event
    // level; the wire-level mapping ("Ready → reason=initialized, NOT
    // restart_succeeded") is covered by host integration tests.
    #[test]
    fn config_added_kind_carries_correct_server_name() {
        let ev = McpClientEvent::ConfigAdded {
            server: "srv".to_string(),
        };
        assert_eq!(ev.server_name(), Some("srv"));
    }
}
