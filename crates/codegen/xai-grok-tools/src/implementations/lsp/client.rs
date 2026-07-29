//! Single LSP server connection — spawn, handshake, protocol methods.

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;

use async_lsp::LanguageServer;
use async_lsp::lsp_types::{
    self, ClientCapabilities, DidChangeTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, GotoCapability, HoverClientCapabilities, InitializeParams,
    InitializedParams, MarkupKind, PublishDiagnosticsClientCapabilities,
    ReferenceClientCapabilities, TextDocumentClientCapabilities, TextDocumentContentChangeEvent,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentSyncClientCapabilities, Url,
    VersionedTextDocumentIdentifier,
};

use super::config::{LspServerConfig, LspTransport};
use super::{DiagnosticsMap, DiagnosticsNotify, LspError, LspMainLoop, file_uri};
use crate::util::{ProcessGroup, ProcessScope};

#[cfg(test)]
use super::config::REQUEST_TIMEOUT;
#[cfg(test)]
use super::format::{flatten_document_symbols, markup_string_to_text};
#[cfg(test)]
use async_lsp::lsp_types::{
    Diagnostic, DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams,
    GotoDefinitionResponse, HoverParams, Location, ReferenceContext, ReferenceParams,
    SymbolInformation, WorkspaceSymbolParams,
};

pub struct LspClient {
    pub server_name: String,
    pub lifecycle_id: u64,
    pub socket: async_lsp::ServerSocket,
    pub diagnostics: DiagnosticsMap,
    pub open_documents: HashMap<String, (i32, String)>,
    pub main_loop: tokio::task::JoinHandle<()>,
    pub stderr_task: Option<tokio::task::JoinHandle<()>>,
    pub child_process: Option<std::process::Child>,
    /// Strong owner of the server child's process group. The session
    /// [`ProcessScope`] holds only a `Weak`, so dropping this on clean teardown
    /// stops the scope from reaping a reused PID. `None` for the socket transport
    /// (no child) or if group creation failed.
    process_group: Option<Arc<ProcessGroup>>,
    pub shutdown_timeout: std::time::Duration,
}

impl std::fmt::Debug for LspClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspClient")
            .field("server_name", &self.server_name)
            .finish_non_exhaustive()
    }
}

impl Drop for LspClient {
    /// Teardown backstop. `LspBackendAdapter`'s graceful shutdown only runs when
    /// a tokio runtime is current, so killing the child here avoids orphaning one
    /// language-server process per session. Idempotent with `shutdown`, which
    /// takes the same fields first.
    fn drop(&mut self) {
        self.reap_children();
    }
}

// ── Startup helpers (called by LspClient::start) ────────────────────────

type LspMainLoopAndServer = (LspMainLoop, async_lsp::ServerSocket);

fn create_client_main_loop(
    diagnostics: DiagnosticsMap,
    diagnostics_notify: DiagnosticsNotify,
) -> LspMainLoopAndServer {
    async_lsp::MainLoop::new_client(|_server_socket| {
        let diag = diagnostics;
        let notify = diagnostics_notify;
        let mut router = async_lsp::router::Router::new(());

        router.notification::<lsp_types::notification::PublishDiagnostics>(
            move |_state, params| {
                let uri_str = params.uri.to_string();
                match diag.write() {
                    Ok(mut map) => {
                        map.insert(uri_str, params.diagnostics);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "diagnostics lock poisoned, dropping update")
                    }
                }
                notify.notify_one();
                ControlFlow::Continue(())
            },
        );

        router.unhandled_notification(|_, _| ControlFlow::Continue(()));
        router
    })
}

type TransportHandles = (
    tokio::task::JoinHandle<()>,
    Option<tokio::task::JoinHandle<()>>,
    Option<std::process::Child>,
);

async fn spawn_transport(
    server_name: &str,
    config: &LspServerConfig,
    main_loop: LspMainLoop,
) -> Result<TransportHandles, LspError> {
    match config.transport {
        LspTransport::Stdio => {
            let (handle, stderr, child) =
                LspClient::start_stdio(server_name, config, main_loop).await?;
            Ok((handle, stderr, Some(child)))
        }
        LspTransport::Socket => {
            let handle = LspClient::start_socket(server_name, config, main_loop).await?;
            Ok((handle, None, None))
        }
    }
}

fn build_initialize_params(config: &LspServerConfig, workspace_root: &Path) -> InitializeParams {
    // Per-server override > session cwd.
    let effective_root = config
        .workspace_folder
        .as_deref()
        .map(Path::new)
        .unwrap_or(workspace_root);

    let workspace_uri = Url::from_file_path(effective_root).ok();
    let workspace_folders = workspace_uri.map(|uri| {
        vec![lsp_types::WorkspaceFolder {
            uri,
            name: effective_root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "workspace".to_string()),
        }]
    });

    #[allow(deprecated)] // root_uri still needed for older servers
    InitializeParams {
        root_uri: Url::from_file_path(effective_root).ok(),
        workspace_folders,
        capabilities: LspClient::client_capabilities(),
        initialization_options: config.initialization_options.clone(),
        ..Default::default()
    }
}

async fn initialize_with_timeout(
    server_name: &str,
    config: &LspServerConfig,
    server: &mut async_lsp::ServerSocket,
    params: InitializeParams,
) -> Result<lsp_types::InitializeResult, LspError> {
    let timeout = std::time::Duration::from_millis(config.startup_timeout_ms());
    match tokio::time::timeout(timeout, server.initialize(params)).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(e)) => Err(LspError::InitFailed(format!("{e}"))),
        Err(_) => Err(LspError::Timeout(server_name.to_string(), timeout)),
    }
}

fn send_initial_configuration(
    server_name: &str,
    config: &LspServerConfig,
    server: &mut async_lsp::ServerSocket,
) {
    if let Some(ref settings) = config.settings
        && let Err(e) = server.did_change_configuration(lsp_types::DidChangeConfigurationParams {
            settings: settings.clone(),
        })
    {
        tracing::warn!(server = %server_name, error = %e, "failed to send didChangeConfiguration");
    }
}

/// Abort the main loop task and kill the child process on startup failure.
fn abort_transport(handle: &tokio::task::JoinHandle<()>, child: &mut Option<std::process::Child>) {
    handle.abort();
    if let Some(c) = child {
        let _ = c.kill();
    }
}

// ── LspClient ───────────────────────────────────────────────────────────

impl LspClient {
    pub async fn start(
        server_name: String,
        lifecycle_id: u64,
        config: LspServerConfig,
        workspace_root: &Path,
        diagnostics_notify: DiagnosticsNotify,
    ) -> Result<Self, LspError> {
        let diagnostics: DiagnosticsMap = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let (main_loop, mut server) =
            create_client_main_loop(diagnostics.clone(), diagnostics_notify);

        let (main_loop_handle, stderr_task, mut child_process) =
            spawn_transport(&server_name, &config, main_loop).await?;

        let init_params = build_initialize_params(&config, workspace_root);

        let init_result =
            match initialize_with_timeout(&server_name, &config, &mut server, init_params).await {
                Ok(result) => result,
                Err(e) => {
                    abort_transport(&main_loop_handle, &mut child_process);
                    return Err(e);
                }
            };

        tracing::info!(
            server = %server_name,
            transport = ?config.transport,
            has_text_sync = init_result.capabilities.text_document_sync.is_some(),
            has_definition = init_result.capabilities.definition_provider.is_some(),
            has_references = init_result.capabilities.references_provider.is_some(),
            "LSP server initialized"
        );

        server
            .initialized(InitializedParams {})
            .map_err(|e| LspError::InitFailed(format!("initialized notification failed: {e}")))?;

        send_initial_configuration(&server_name, &config, &mut server);

        tokio::task::yield_now().await;

        Ok(Self {
            server_name,
            lifecycle_id,
            socket: server,
            diagnostics,
            open_documents: HashMap::new(),
            main_loop: main_loop_handle,
            stderr_task,
            child_process,
            process_group: None,
            shutdown_timeout: std::time::Duration::from_millis(config.shutdown_timeout_ms()),
        })
    }

    /// Install a process group for this freshly started stdio server: register a
    /// `Weak` into the session [`ProcessScope`] (when set) while this client keeps
    /// the strong `Arc`; installed even without a scope so this client's own
    /// `Drop` killpg's the whole child tree. No-op for the socket transport.
    /// See the `process_group` field doc for the Weak/reuse-safety argument.
    ///
    /// Returns `false` when the scope was already closed (session teardown raced
    /// this start): the child has been killed at registration, so the caller must
    /// discard this client instead of installing it as ready.
    pub(crate) fn enroll(&mut self, scope: Option<&ProcessScope>) -> bool {
        let Some(child) = self.child_process.as_ref() else {
            return true;
        };
        // Group-creation failures degrade to leader-only cleanup and exempt this
        // server from session-close reaping — actionable and otherwise invisible,
        // so they warrant `warn`.
        let mut group = match ProcessGroup::new() {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(server = %self.server_name, pid = child.id(), error = %e, "LSP: ProcessGroup::new failed; server exempt from session reaping");
                return true;
            }
        };
        if let Err(e) = group.attach_std(child) {
            tracing::warn!(server = %self.server_name, pid = child.id(), error = %e, "LSP: attach to process group failed; server exempt from session reaping");
            return true;
        }
        let group = Arc::new(group);
        let enrolled = match scope {
            Some(scope) => scope.register(&group),
            None => true,
        };
        // Keep the strong Arc either way so `Drop`/`shutdown` reap the tree —
        // including the already-killed leader in the closed-scope case.
        self.process_group = Some(group);
        enrolled
    }

    async fn start_stdio(
        server_name: &str,
        config: &LspServerConfig,
        main_loop: LspMainLoop,
    ) -> Result<
        (
            tokio::task::JoinHandle<()>,
            Option<tokio::task::JoinHandle<()>>,
            std::process::Child,
        ),
        LspError,
    > {
        let mut cmd = std::process::Command::new(&config.command);
        cmd.args(&config.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for (k, v) in &config.env {
            cmd.env(k, v);
        }
        xai_tty_utils::detach_std_command(&mut cmd);
        cmd.envs(xai_tty_utils::pager_env());
        let mut child = cmd
            .spawn()
            .map_err(|e| LspError::SpawnFailed(format!("'{}': {e}", config.command)))?;

        let child_stdout = child
            .stdout
            .take()
            .ok_or_else(|| LspError::SpawnFailed("no stdout".into()))?;
        let child_stdin = child
            .stdin
            .take()
            .ok_or_else(|| LspError::SpawnFailed("no stdin".into()))?;

        let stderr_task = child.stderr.take().map(|stderr| {
            let name = server_name.to_string();
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let stderr = tokio::process::ChildStderr::from_std(stderr);
                let Ok(stderr) = stderr else { return };
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(server = %name, "stderr: {line}");
                }
            })
        });

        tracing::debug!(server = %server_name, pid = ?child.id(), "LSP server spawned (stdio)");

        let async_stdout = tokio::process::ChildStdout::from_std(child_stdout)
            .map_err(|e| LspError::SpawnFailed(format!("stdout async wrap: {e}")))?;
        let async_stdin = tokio::process::ChildStdin::from_std(child_stdin)
            .map_err(|e| LspError::SpawnFailed(format!("stdin async wrap: {e}")))?;

        let name = server_name.to_string();
        let handle = tokio::spawn(async move {
            use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
            if let Err(e) = main_loop
                .run_buffered(async_stdout.compat(), async_stdin.compat_write())
                .await
            {
                tracing::warn!(server = %name, error = %e, "LSP main loop exited with error");
            }
        });
        Ok((handle, stderr_task, child))
    }

    /// Connect to an LSP server over TCP socket.
    /// Uses `command` as the `host:port` address.
    async fn start_socket(
        server_name: &str,
        config: &LspServerConfig,
        main_loop: LspMainLoop,
    ) -> Result<tokio::task::JoinHandle<()>, LspError> {
        let addr = &config.command;

        tracing::debug!(server = %server_name, %addr, "connecting to LSP server (socket)");

        let stream = tokio::net::TcpStream::connect(&addr)
            .await
            .map_err(|e| LspError::SpawnFailed(format!("TCP connect to '{addr}': {e}")))?;

        tracing::debug!(server = %server_name, %addr, "LSP server connected (socket)");

        let (read_half, write_half) = stream.into_split();
        let name = server_name.to_string();
        Ok(tokio::spawn(async move {
            use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
            if let Err(e) = main_loop
                .run_buffered(read_half.compat(), write_half.compat_write())
                .await
            {
                tracing::warn!(server = %name, error = %e, "LSP main loop exited with error (socket)");
            }
        }))
    }

    pub fn close_all_documents(&mut self) {
        for (uri_str, _version) in std::mem::take(&mut self.open_documents) {
            let Ok(uri) = Url::parse(&uri_str) else {
                continue;
            };
            tracing::debug!(server = %self.server_name, %uri, "didClose");
            if let Err(e) = self
                .socket
                .did_close(lsp_types::DidCloseTextDocumentParams {
                    text_document: TextDocumentIdentifier { uri },
                })
            {
                tracing::debug!(server = %self.server_name, error = %e, "failed to send didClose");
            }
        }
    }

    pub async fn shutdown(mut self) {
        // Dead transport — a crashed server, or the session scope's
        // SIGKILL-on-close (see grok-shell `take_session`) landing before this
        // Drop-spawned graceful task ran. The shutdown/exit handshake can only
        // fail, so skip it (and its warnings) and just reap.
        if self.main_loop.is_finished() {
            tracing::debug!(server = %self.server_name, "LSP transport already down; skipping shutdown handshake");
            self.reap_children();
            return;
        }
        self.close_all_documents();

        let result = tokio::time::timeout(self.shutdown_timeout, async {
            if let Err(e) = self.socket.shutdown(()).await {
                tracing::warn!(server = %self.server_name, error = %e, "LSP shutdown request failed");
            }
            if let Err(e) = self.socket.exit(()) {
                tracing::warn!(server = %self.server_name, error = %e, "LSP exit notification failed");
            }
        })
        .await;

        if result.is_err() {
            tracing::warn!(
                server = %self.server_name,
                timeout_ms = self.shutdown_timeout.as_millis() as u64,
                "LSP shutdown timed out, aborting main loop"
            );
            self.main_loop.abort();
        }

        // `&mut`-await (not move) so `self` is never partially moved: `LspClient`
        // has a `Drop` impl, and you cannot move fields out of a `Drop` type.
        if let Err(e) = (&mut self.main_loop).await
            && !e.is_cancelled()
        {
            tracing::warn!(server = %self.server_name, error = %e, "LSP main loop task panicked");
        }

        self.reap_children();
    }

    /// Shared teardown for `Drop` and `shutdown`: abort the tasks, then reap
    /// grandchildren via the group before the leader. Idempotent via `take`, so
    /// running it from both paths is safe.
    fn reap_children(&mut self) {
        self.main_loop.abort();
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
        if let Some(group) = self.process_group.take() {
            let _ = group.kill();
        }
        if let Some(mut child) = self.child_process.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn client_capabilities() -> ClientCapabilities {
        ClientCapabilities {
            text_document: Some(TextDocumentClientCapabilities {
                synchronization: Some(TextDocumentSyncClientCapabilities {
                    dynamic_registration: Some(false),
                    will_save: Some(false),
                    will_save_wait_until: Some(false),
                    did_save: Some(true),
                }),
                definition: Some(GotoCapability {
                    dynamic_registration: Some(false),
                    link_support: Some(false),
                }),
                references: Some(ReferenceClientCapabilities {
                    dynamic_registration: Some(false),
                }),
                publish_diagnostics: Some(PublishDiagnosticsClientCapabilities {
                    related_information: Some(true),
                    ..Default::default()
                }),
                hover: Some(HoverClientCapabilities {
                    dynamic_registration: Some(false),
                    content_format: Some(vec![MarkupKind::PlainText]),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Returns (uri_string, language_id) for all documents this client has opened.
    pub fn tracked_documents(&self) -> Vec<(String, String)> {
        self.open_documents
            .iter()
            .map(|(uri, (_, lang_id))| (uri.clone(), lang_id.clone()))
            .collect()
    }

    #[cfg(test)]
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    pub fn notify_file_change(&mut self, path: &Path, content: &str, language_id: &str) {
        let uri = match file_uri(path) {
            Ok(u) => u,
            Err(_) => {
                tracing::warn!(server = %self.server_name,"skipping didOpen/didChange: invalid path");
                return;
            }
        };
        let uri_str = uri.to_string();

        let (is_new, version) = match self.open_documents.get_mut(&uri_str) {
            Some((v, _)) => {
                *v += 1;
                (false, *v)
            }
            None => {
                self.open_documents
                    .insert(uri_str, (0, language_id.to_string()));
                (true, 0)
            }
        };

        if is_new {
            tracing::debug!(server = %self.server_name, uri = %uri, language_id, "didOpen");
            if let Err(e) = self.socket.did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: language_id.to_string(),
                    version,
                    text: content.to_string(),
                },
            }) {
                tracing::debug!(server = %self.server_name, error = %e, "failed to send didOpen");
            }
        } else {
            tracing::debug!(server = %self.server_name, uri = %uri, version, "didChange");
            if let Err(e) = self.socket.did_change(DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: uri.clone(),
                    version,
                },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: content.to_string(),
                }],
            }) {
                tracing::debug!(server = %self.server_name, error = %e, "failed to send didChange");
            }
        }

        // Some servers only emit diagnostics on save, not change.
        if let Err(e) = self.socket.did_save(DidSaveTextDocumentParams {
            text_document: TextDocumentIdentifier { uri },
            text: Some(content.to_string()),
        }) {
            tracing::debug!(server = %self.server_name, error = %e, "failed to send didSave");
        }
    }

    #[cfg(test)]
    pub fn get_diagnostics(&self, path: &Path) -> Vec<Diagnostic> {
        let uri = match file_uri(path) {
            Ok(u) => u,
            Err(_) => return vec![],
        };
        self.diagnostics
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&uri.to_string())
            .cloned()
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub async fn goto_definition(
        &mut self,
        path: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<Location>, LspError> {
        let params = GotoDefinitionParams {
            text_document_position_params: super::text_document_position(path, line, column)?,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        let result = tokio::time::timeout(REQUEST_TIMEOUT, self.socket.definition(params))
            .await
            .map_err(|_| LspError::RequestFailed("request timed out".into()))?
            .map_err(|e| LspError::RequestFailed(format!("{e}")))?;

        Ok(match result {
            Some(GotoDefinitionResponse::Scalar(loc)) => vec![loc],
            Some(GotoDefinitionResponse::Array(locs)) => locs,
            Some(GotoDefinitionResponse::Link(links)) => links
                .into_iter()
                .map(|link| Location {
                    uri: link.target_uri,
                    range: link.target_selection_range,
                })
                .collect(),
            None => vec![],
        })
    }

    #[cfg(test)]
    pub async fn goto_implementation(
        &mut self,
        path: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<Location>, LspError> {
        let params = GotoDefinitionParams {
            text_document_position_params: super::text_document_position(path, line, column)?,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        let result = tokio::time::timeout(REQUEST_TIMEOUT, self.socket.implementation(params))
            .await
            .map_err(|_| LspError::RequestFailed("request timed out".into()))?
            .map_err(|e| LspError::RequestFailed(format!("{e}")))?;

        Ok(match result {
            Some(GotoDefinitionResponse::Scalar(loc)) => vec![loc],
            Some(GotoDefinitionResponse::Array(locs)) => locs,
            Some(GotoDefinitionResponse::Link(links)) => links
                .into_iter()
                .map(|link| Location {
                    uri: link.target_uri,
                    range: link.target_selection_range,
                })
                .collect(),
            None => vec![],
        })
    }

    #[cfg(test)]
    pub async fn goto_references(
        &mut self,
        path: &Path,
        line: u32,
        column: u32,
    ) -> Result<Vec<Location>, LspError> {
        let params = ReferenceParams {
            text_document_position: super::text_document_position(path, line, column)?,
            context: ReferenceContext {
                include_declaration: true,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        let result = tokio::time::timeout(REQUEST_TIMEOUT, self.socket.references(params))
            .await
            .map_err(|_| LspError::RequestFailed("request timed out".into()))?
            .map_err(|e| LspError::RequestFailed(format!("{e}")))?;

        Ok(result.unwrap_or_default())
    }

    #[cfg(test)]
    pub async fn hover(
        &mut self,
        path: &Path,
        line: u32,
        column: u32,
    ) -> Result<Option<String>, LspError> {
        let params = HoverParams {
            text_document_position_params: super::text_document_position(path, line, column)?,
            work_done_progress_params: Default::default(),
        };

        let result = tokio::time::timeout(REQUEST_TIMEOUT, self.socket.hover(params))
            .await
            .map_err(|_| LspError::RequestFailed("request timed out".into()))?
            .map_err(|e| LspError::RequestFailed(format!("{e}")))?;

        Ok(result.map(|hover| match hover.contents {
            lsp_types::HoverContents::Scalar(ms) => markup_string_to_text(ms),
            lsp_types::HoverContents::Array(arr) => arr
                .into_iter()
                .map(markup_string_to_text)
                .collect::<Vec<_>>()
                .join("\n"),
            lsp_types::HoverContents::Markup(mc) => mc.value,
        }))
    }

    #[cfg(test)]
    pub async fn document_symbols(
        &mut self,
        path: &Path,
    ) -> Result<Vec<SymbolInformation>, LspError> {
        let params = DocumentSymbolParams {
            text_document: TextDocumentIdentifier {
                uri: file_uri(path)?,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        let result = tokio::time::timeout(REQUEST_TIMEOUT, self.socket.document_symbol(params))
            .await
            .map_err(|_| LspError::RequestFailed("request timed out".into()))?
            .map_err(|e| LspError::RequestFailed(format!("{e}")))?;

        Ok(match result {
            Some(DocumentSymbolResponse::Flat(symbols)) => symbols,
            Some(DocumentSymbolResponse::Nested(nested)) => {
                let mut flat = Vec::new();
                let uri = file_uri(path)?;
                flatten_document_symbols(&nested, &uri, &mut flat);
                flat
            }
            None => vec![],
        })
    }

    #[cfg(test)]
    pub async fn workspace_symbols(
        &mut self,
        query: &str,
    ) -> Result<Vec<SymbolInformation>, LspError> {
        let params = WorkspaceSymbolParams {
            query: query.to_string(),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        let result = tokio::time::timeout(REQUEST_TIMEOUT, self.socket.symbol(params))
            .await
            .map_err(|_| LspError::RequestFailed("request timed out".into()))?
            .map_err(|e| LspError::RequestFailed(format!("{e}")))?;

        Ok(match result {
            Some(lsp_types::WorkspaceSymbolResponse::Flat(symbols)) => symbols,
            Some(lsp_types::WorkspaceSymbolResponse::Nested(ws_list)) => {
                // Convert WorkspaceSymbol to SymbolInformation (lossy but usable)
                ws_list
                    .into_iter()
                    .map(|ws| {
                        let loc = match ws.location {
                            lsp_types::OneOf::Left(loc) => loc,
                            lsp_types::OneOf::Right(doc_loc) => Location {
                                uri: doc_loc.uri,
                                range: Default::default(),
                            },
                        };
                        #[allow(deprecated)]
                        SymbolInformation {
                            name: ws.name,
                            kind: ws.kind,
                            tags: ws.tags,
                            deprecated: None,
                            location: loc,
                            container_name: ws.container_name,
                        }
                    })
                    .collect()
            }
            None => vec![],
        })
    }
}
