//! Manages multiple LSP servers, routes by file extension, collects diagnostics.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_lsp::lsp_types::{DiagnosticSeverity, Url};

use super::client::LspClient;
use super::config::LspServerConfig;
use super::{DiagnosticsNotify, file_uri};
use crate::util::ProcessScope;

#[cfg(test)]
use super::format::{format_locations_labeled, format_symbols};

pub struct DiagnosticsSummary {
    pub text: String,
    pub file_count: usize,
    pub diagnostic_count: usize,
}

#[derive(Default)]
pub struct PendingDiagnosticsState {
    lifecycle_id: u64,
    uris: BTreeSet<String>,
}

/// Result of `collect_pending_diagnostics` — pure data, no state mutation.
#[derive(Default)]
struct CollectedDiagnostics {
    lines: Vec<String>,
    file_count: usize,
    diagnostic_count: usize,
    servers_without_diagnostics: Vec<String>,
}

pub struct LspManager {
    pub servers: BTreeMap<String, LspServerConfig>,
    pub clients: HashMap<String, LspClient>,
    pub workspace_root: PathBuf,
    pub initialized: bool,
    pub tools_enabled: bool,
    pub pending_diagnostics_by_server: HashMap<String, PendingDiagnosticsState>,
    pub diagnostics_ready: DiagnosticsNotify,
    pub shutting_down: bool,
    pub next_lifecycle_id: u64,
    pub notification_handle: crate::notification::ToolNotificationHandle,
    /// When set, each spawned server's process group is registered here so the
    /// agent can reap the language-server trees on session close. `None` outside
    /// an agent session.
    pub process_scope: Option<ProcessScope>,
}

impl Default for LspManager {
    fn default() -> Self {
        Self {
            servers: BTreeMap::new(),
            clients: HashMap::new(),
            workspace_root: PathBuf::from("/tmp"),
            initialized: false,
            tools_enabled: false,
            pending_diagnostics_by_server: HashMap::new(),
            diagnostics_ready: Arc::new(tokio::sync::Notify::new()),
            shutting_down: false,
            next_lifecycle_id: 1,
            notification_handle: crate::notification::ToolNotificationHandle::noop(),
            process_scope: None,
        }
    }
}

impl LspManager {
    pub fn new(
        servers: BTreeMap<String, LspServerConfig>,
        workspace_root: PathBuf,
        tools_enabled: bool,
        notification_handle: crate::notification::ToolNotificationHandle,
    ) -> Self {
        Self {
            servers,
            workspace_root,
            tools_enabled,
            notification_handle,
            ..Self::default()
        }
    }

    /// Attach the session process scope so spawned servers are enrolled for
    /// reclaim.
    pub fn with_process_scope(mut self, scope: Option<ProcessScope>) -> Self {
        self.process_scope = scope;
        self
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub fn alloc_lifecycle_id(&mut self) -> u64 {
        let lifecycle_id = self.next_lifecycle_id;
        self.next_lifecycle_id = self.next_lifecycle_id.saturating_add(1);
        lifecycle_id
    }

    pub fn mark_uri_pending_diagnostics(&mut self, server_name: &str, lifecycle_id: u64, uri: Url) {
        let pending = self
            .pending_diagnostics_by_server
            .entry(server_name.to_string())
            .or_default();
        if pending.lifecycle_id != lifecycle_id {
            pending.lifecycle_id = lifecycle_id;
            pending.uris.clear();
        }
        pending.uris.insert(uri.to_string());
    }

    pub fn mark_path_pending_diagnostics(
        &mut self,
        server_name: &str,
        lifecycle_id: u64,
        path: &Path,
    ) {
        if let Ok(uri) = file_uri(path) {
            self.mark_uri_pending_diagnostics(server_name, lifecycle_id, uri);
        }
    }

    pub async fn ensure_initialized(&mut self) {
        if self.initialized {
            return;
        }
        self.initialized = true;

        let configs: Vec<_> = self
            .servers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (name, server_config) in configs {
            self.notification_handle
                .send_lsp_starting(crate::notification::LspServerStarting {
                    server_name: name.clone(),
                    command: server_config.command.clone(),
                });
            let lifecycle_id = self.alloc_lifecycle_id();
            match LspClient::start(
                name.clone(),
                lifecycle_id,
                server_config,
                &self.workspace_root,
                self.diagnostics_ready.clone(),
            )
            .await
            {
                Ok(mut client) => {
                    if !client.enroll(self.process_scope.as_ref()) {
                        // Session teardown raced this start: the closed scope
                        // killed the child at registration. Installing the
                        // client would advertise a dead server and feed the
                        // restart monitor respawn churn, so stop starting
                        // servers for this manager instead.
                        tracing::info!(server = %name, "session scope closed during LSP start; discarding server");
                        self.shutting_down = true;
                        return;
                    }
                    // Same race as the restart path: `kill_all` landing between
                    // enroll and insert has already SIGKILLed the child.
                    if self.process_scope.as_ref().is_some_and(|s| s.is_closed()) {
                        tracing::info!(server = %name, "session scope closed during LSP start; discarding server");
                        self.shutting_down = true;
                        return;
                    }
                    tracing::info!(server = %name, "LSP server ready");
                    self.notification_handle
                        .send_lsp_ready(crate::notification::LspServerReady {
                            server_name: name.clone(),
                        });
                    self.clients.insert(name, client);
                }
                Err(e) => {
                    tracing::warn!(server = %name, error = %e, "failed to start LSP server, skipping");
                    self.notification_handle.send_lsp_failed(
                        crate::notification::LspServerFailed {
                            server_name: name.clone(),
                            error: e.to_string(),
                            attempts: 0,
                        },
                    );
                }
            }
        }
    }

    pub async fn shutdown(&mut self) {
        self.shutting_down = true;
        for (name, client) in self.clients.drain() {
            tracing::info!(server = %name, "shutting down LSP server");
            client.shutdown().await;
        }
    }

    pub fn tools_enabled(&self) -> bool {
        self.tools_enabled && !self.clients.is_empty()
    }

    pub fn restartable_servers(&self) -> Vec<String> {
        self.servers
            .iter()
            .filter(|(name, cfg)| cfg.restart_on_crash() && self.clients.contains_key(*name))
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Only called after `search_replace`; other mutations (bash, git) are not tracked.
    pub fn notify_file_changed(&mut self, path: &Path, content: &str) {
        let (server_name, lang_id) = match super::config::resolve_server(&self.servers, path) {
            Some(pair) => pair,
            None => return,
        };
        let client = match self.clients.get_mut(&server_name) {
            Some(c) => c,
            None => return,
        };
        let lifecycle_id = client.lifecycle_id;
        client.notify_file_change(path, content, &lang_id);
        self.mark_path_pending_diagnostics(&server_name, lifecycle_id, path);
    }

    pub fn has_pending_diagnostics(&self) -> bool {
        self.pending_diagnostics_by_server
            .values()
            .any(|pending| !pending.uris.is_empty())
    }

    fn pending_file_count(&self) -> usize {
        self.pending_diagnostics_by_server
            .values()
            .map(|pending| pending.uris.len())
            .sum()
    }

    #[cfg(test)]
    pub fn pending_count(&self) -> usize {
        self.pending_file_count()
    }

    #[cfg(test)]
    pub fn is_uri_pending(&self, path: &Path) -> bool {
        file_uri(path)
            .map(|uri| {
                self.pending_diagnostics_by_server
                    .values()
                    .any(|pending| pending.uris.contains(uri.as_str()))
            })
            .unwrap_or(false)
    }

    fn build_pending_diagnostics_summary(&mut self) -> Option<DiagnosticsSummary> {
        if !self.has_pending_diagnostics() {
            return None;
        }

        let collected = self.collect_pending_diagnostics();

        if collected.lines.is_empty() {
            tracing::debug!(
                pending_servers = collected.servers_without_diagnostics.len(),
                pending_file_count = self.pending_file_count(),
                servers = ?collected.servers_without_diagnostics,
                "no LSP diagnostics available for pending files"
            );
            None
        } else {
            self.pending_diagnostics_by_server.clear();
            Some(DiagnosticsSummary {
                text: format!(
                    "<lsp-diagnostics>\n{}\n</lsp-diagnostics>",
                    collected.lines.join("\n")
                ),
                file_count: collected.file_count,
                diagnostic_count: collected.diagnostic_count,
            })
        }
    }

    /// Pure data collection — reads from clients and pending state without mutation.
    fn collect_pending_diagnostics(&self) -> CollectedDiagnostics {
        let mut result = CollectedDiagnostics::default();

        for (server_name, pending) in &self.pending_diagnostics_by_server {
            let Some(client) = self.clients.get(server_name) else {
                continue;
            };
            if client.lifecycle_id != pending.lifecycle_id {
                continue;
            }

            let map = client.diagnostics.read().unwrap_or_else(|e| e.into_inner());
            let mut server_had_diagnostics = false;

            for uri in &pending.uris {
                let Some(diags) = map.get(uri.as_str()) else {
                    continue;
                };
                server_had_diagnostics = true;
                let display_path = uri.strip_prefix("file://").unwrap_or(uri); // Unix-only
                let mut has_header = false;

                for d in diags {
                    let label = match d.severity {
                        Some(DiagnosticSeverity::ERROR) => "error",
                        Some(DiagnosticSeverity::WARNING) => "warn",
                        _ => continue,
                    };
                    if !has_header {
                        result.lines.push(format!("{display_path}:"));
                        has_header = true;
                        result.file_count += 1;
                    }
                    result.diagnostic_count += 1;
                    let msg = d
                        .message
                        .replace("</lsp-diagnostics>", "&lt;/lsp-diagnostics&gt;")
                        .replace("</system-reminder>", "&lt;/system-reminder&gt;");
                    result
                        .lines
                        .push(format!("  {label}[L{}]: {msg}", d.range.start.line + 1));
                }
            }

            if !server_had_diagnostics {
                result.servers_without_diagnostics.push(server_name.clone());
            }
        }

        result
    }

    /// Auto-open file if needed, return cloned socket for lock-free dispatch.
    /// Single resolve — no double lookup.
    pub async fn socket_for_file(&mut self, path: &Path) -> Option<async_lsp::ServerSocket> {
        let (server_name, lang_id) = super::config::resolve_server(&self.servers, path)?;
        // Auto-open if not yet tracked.
        let needs_open = self
            .clients
            .get(&server_name)
            .and_then(|c| {
                file_uri(path)
                    .ok()
                    .map(|uri| !c.open_documents.contains_key(&uri.to_string()))
            })
            .unwrap_or(false);
        if needs_open
            && let Ok(content) = tokio::fs::read_to_string(path).await
            && let Some(client) = self.clients.get_mut(&server_name)
        {
            client.notify_file_change(path, &content, &lang_id);
            tokio::task::yield_now().await;
        }
        self.clients.get(&server_name).map(|c| c.socket.clone())
    }

    pub fn all_sockets(&self) -> Vec<async_lsp::ServerSocket> {
        self.clients.values().map(|c| c.socket.clone()).collect()
    }

    #[cfg(test)]
    pub fn client_for_file_mut(&mut self, path: &Path) -> Option<&mut LspClient> {
        let (server_name, _) = super::config::resolve_server(&self.servers, path)?;
        self.clients.get_mut(&server_name)
    }

    #[cfg(test)]
    pub async fn dispatch_tool_typed(
        &mut self,
        input: &super::LspToolInput,
    ) -> super::LspToolResult {
        use super::{LspOperation, LspToolResult};

        let err = |msg: String| LspToolResult {
            text: msg,
            is_error: true,
        };

        let result = match input.operation {
            LspOperation::GoToDefinition
            | LspOperation::FindReferences
            | LspOperation::Hover
            | LspOperation::GoToImplementation => {
                let (Some(fp), Some(line), Some(col)) =
                    (&input.file_path, input.line, input.character)
                else {
                    return err("Required: file_path (string), line (int), character (int).".into());
                };
                let path = PathBuf::from(fp);
                let Some(client) = self.client_for_file_mut(&path) else {
                    return err(format!("No LSP server configured for {}", path.display()));
                };
                match input.operation {
                    LspOperation::GoToDefinition => client
                        .goto_definition(&path, line, col)
                        .await
                        .map(|l| format_locations_labeled("Definition", &l)),
                    LspOperation::FindReferences => client
                        .goto_references(&path, line, col)
                        .await
                        .map(|l| format_locations_labeled("References", &l)),
                    LspOperation::GoToImplementation => client
                        .goto_implementation(&path, line, col)
                        .await
                        .map(|l| format_locations_labeled("Implementations", &l)),
                    LspOperation::Hover => client.hover(&path, line, col).await.map(|opt| {
                        opt.unwrap_or_else(|| "No hover information available.".to_string())
                    }),
                    _ => unreachable!(),
                }
            }
            LspOperation::DocumentSymbol => {
                let Some(ref file_path) = input.file_path else {
                    return err("Required: file_path (string).".into());
                };
                let path = PathBuf::from(file_path);
                let Some(client) = self.client_for_file_mut(&path) else {
                    return err(format!("No LSP server configured for {}", path.display()));
                };
                client
                    .document_symbols(&path)
                    .await
                    .map(|s| format_symbols(&s))
            }
            LspOperation::WorkspaceSymbol => {
                let Some(ref query) = input.query else {
                    return err("Required: query (string).".into());
                };
                if self.clients.is_empty() {
                    return err("No LSP servers are running.".into());
                }
                let mut all_symbols = Vec::new();
                let mut last_err = None;
                for client in self.clients.values_mut() {
                    match client.workspace_symbols(query).await {
                        Ok(symbols) => all_symbols.extend(symbols),
                        Err(e) => {
                            last_err = Some(e);
                        }
                    }
                }
                if all_symbols.is_empty() {
                    match last_err {
                        Some(e) => Err(e),
                        None => Ok(format_symbols(&[])),
                    }
                } else {
                    Ok(format_symbols(&all_symbols))
                }
            }
        };

        match result {
            Ok(text) => LspToolResult {
                text,
                is_error: false,
            },
            Err(e) => {
                tracing::warn!(error = %e, "LSP tool failed");
                LspToolResult {
                    text: format!("LSP error: {e}"),
                    is_error: true,
                }
            }
        }
    }
}

/// Drops the lock during the Notify wait so `notify_file_changed` isn't blocked.
pub async fn drain_lsp_diagnostics(
    lsp_manager: &tokio::sync::Mutex<LspManager>,
    timeout: std::time::Duration,
) -> Option<DiagnosticsSummary> {
    let mut lsp = lsp_manager.lock().await;
    if !lsp.has_pending_diagnostics() {
        return None;
    }

    if let Some(summary) = lsp.build_pending_diagnostics_summary() {
        return Some(summary);
    }

    // Register waiter before dropping lock so notify_one() isn't lost.
    let notify = lsp.diagnostics_ready.clone();
    let notified = notify.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    drop(lsp);

    let _ = tokio::time::timeout(timeout, &mut notified).await;

    let mut lsp = lsp_manager.lock().await;
    let result = lsp.build_pending_diagnostics_summary();
    if result.is_none() {
        tracing::debug!(
            pending_file_count = lsp.pending_file_count(),
            timeout_ms = timeout.as_millis() as u64,
            "LSP diagnostics not available after timeout, preserving pending state"
        );
    }
    result
}
