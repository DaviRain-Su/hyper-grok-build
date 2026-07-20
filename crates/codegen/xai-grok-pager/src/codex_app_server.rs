//! Async client for the local `codex app-server` JSONL protocol.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::Path,
    rc::Rc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{broadcast, mpsc, oneshot},
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub(crate) struct CodexReasoningEffort {
    pub value: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct CodexModel {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub is_default: bool,
    pub supported_reasoning_efforts: Vec<CodexReasoningEffort>,
    pub default_reasoning_effort: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct CodexStatus {
    pub account_label: String,
    pub models: Vec<CodexModel>,
    /// Subscription's current default model id, if any. Falls back to the
    /// first listed model when the server does not flag one.
    pub default_model: Option<String>,
}

/// Cloneable app-server handle. A permanent reader resolves requests and fans
/// notifications out to active turns, so cancellation never contends for the
/// stdout reader held by a prompt.
pub(crate) struct CodexAppServer {
    _child: Child,
    writer: mpsc::UnboundedSender<Value>,
    pending: Rc<RefCell<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>,
    notifications: broadcast::Sender<Value>,
    next_id: Cell<u64>,
}

impl CodexAppServer {
    pub(crate) async fn start(binary: &Path) -> Result<Self> {
        let mut command = Command::new(binary);
        command
            .arg("app-server")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().with_context(|| {
            format!(
                "could not start Codex CLI at '{}'; install @openai/codex and run `codex login`",
                binary.display()
            )
        })?;
        let mut stdin = child
            .stdin
            .take()
            .context("Codex app-server stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("Codex app-server stdout unavailable")?;
        if let Some(mut stderr) = child.stderr.take() {
            tokio::task::spawn_local(async move {
                let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
            });
        }

        let (writer, mut writes) = mpsc::unbounded_channel::<Value>();
        tokio::task::spawn_local(async move {
            while let Some(value) = writes.recv().await {
                let Ok(mut encoded) = serde_json::to_vec(&value) else {
                    continue;
                };
                encoded.push(b'\n');
                if stdin.write_all(&encoded).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
        });

        let pending = Rc::new(RefCell::new(HashMap::<
            u64,
            oneshot::Sender<Result<Value, String>>,
        >::new()));
        let (notifications, _) = broadcast::channel(1024);
        let reader_pending = pending.clone();
        let reader_notifications = notifications.clone();
        let reader_writer = writer.clone();
        tokio::task::spawn_local(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let Some(id) = message.get("id").and_then(Value::as_u64)
                    && message.get("method").is_none()
                {
                    if let Some(tx) = reader_pending.borrow_mut().remove(&id) {
                        let result = message
                            .get("error")
                            .map(|error| Err(format_rpc_error(error)))
                            .unwrap_or_else(|| {
                                Ok(message.get("result").cloned().unwrap_or(Value::Null))
                            });
                        let _ = tx.send(result);
                    }
                    continue;
                }
                if is_server_request(&message) {
                    answer_server_request(&reader_writer, &message);
                    continue;
                }
                if message.get("method").is_some() {
                    let _ = reader_notifications.send(message);
                }
            }
            let mut pending = reader_pending.borrow_mut();
            for (_, tx) in pending.drain() {
                let _ = tx.send(Err("Codex app-server exited unexpectedly".to_owned()));
            }
        });

        let server = Self {
            _child: child,
            writer,
            pending,
            notifications,
            next_id: Cell::new(1),
        };
        server
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "grok_cli",
                        "title": "Grok Build",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": { "experimentalApi": true }
                }),
            )
            .await?;
        server.notify("initialized", json!({}))?;
        Ok(server)
    }

    pub(crate) async fn status(&self) -> Result<CodexStatus> {
        let account = self.request("account/read", json!({})).await?;
        let account = account
            .get("account")
            .filter(|value| !value.is_null())
            .ok_or_else(|| anyhow!("Codex CLI is not authenticated; run `codex login`"))?;
        let account_label = match account.get("type").and_then(Value::as_str) {
            Some("chatgpt") => format!(
                "ChatGPT {} subscription",
                account
                    .get("planType")
                    .and_then(Value::as_str)
                    .unwrap_or("authenticated")
            ),
            Some("apiKey") => "OpenAI API key".to_owned(),
            Some(other) => other.to_owned(),
            None => "authenticated Codex account".to_owned(),
        };

        let mut models = Vec::new();
        let mut default_model: Option<String> = None;
        let mut cursor: Option<String> = None;
        loop {
            let response = self
                .request(
                    "model/list",
                    cursor
                        .as_ref()
                        .map_or_else(|| json!({}), |cursor| json!({ "cursor": cursor })),
                )
                .await?;
            for model in response
                .get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(id) = model.get("model").and_then(Value::as_str) else {
                    continue;
                };
                if model.get("isDefault").and_then(Value::as_bool) == Some(true) {
                    default_model = Some(id.to_owned());
                }
                models.push(CodexModel {
                    id: id.to_owned(),
                    name: model
                        .get("displayName")
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_owned(),
                    description: model
                        .get("description")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    is_default: model.get("isDefault").and_then(Value::as_bool) == Some(true),
                    supported_reasoning_efforts: model
                        .get("supportedReasoningEfforts")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|effort| {
                            let value = effort
                                .as_str()
                                .or_else(|| effort.get("reasoningEffort")?.as_str())
                                .map(ToOwned::to_owned)?;
                            Some(CodexReasoningEffort {
                                value,
                                description: effort
                                    .get("description")
                                    .and_then(Value::as_str)
                                    .map(ToOwned::to_owned),
                            })
                        })
                        .collect(),
                    default_reasoning_effort: model
                        .get("defaultReasoningEffort")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                });
            }
            cursor = response
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        if default_model.is_none() {
            default_model = models.first().map(|model| model.id.clone());
        }
        Ok(CodexStatus {
            account_label,
            models,
            default_model,
        })
    }

    pub(crate) async fn start_thread(&self, cwd: &Path, model: &str) -> Result<String> {
        self.open_thread(cwd, Some(model), None, "workspace-write")
            .await
    }

    /// Open or resume a Codex thread with an explicit sandbox mode.
    ///
    /// `sandbox` is the thread/start `sandbox` string (`"workspace-write"` or
    /// `"danger-full-access"`). When `resume` is `Some`, the server is asked
    /// to resume an existing thread id instead of starting a new one.
    pub(crate) async fn open_thread(
        &self,
        cwd: &Path,
        model: Option<&str>,
        resume: Option<&str>,
        sandbox: &str,
    ) -> Result<String> {
        let mut params = json!({
            "cwd": cwd,
            "approvalPolicy": "never",
            "sandbox": sandbox,
        });
        if let Some(model) = model {
            params["model"] = json!(model);
        }
        let method = if resume.is_some() {
            "thread/resume"
        } else {
            "thread/start"
        };
        if let Some(thread_id) = resume {
            params["threadId"] = json!(thread_id);
        }
        let response = self.request(method, params).await?;
        response
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| anyhow!("Codex app-server did not return a thread id"))
    }

    pub(crate) async fn start_turn(
        &self,
        thread_id: &str,
        input: Vec<Value>,
        model: &str,
        effort: Option<&str>,
        cwd: &Path,
    ) -> Result<String> {
        self.start_turn_with(
            thread_id,
            input,
            Some(model),
            effort,
            workspace_write_sandbox(cwd),
        )
        .await
    }

    /// Start a turn with an explicit sandbox policy. `model` is optional so
    /// callers that want the server default can omit it.
    pub(crate) async fn start_turn_with(
        &self,
        thread_id: &str,
        input: Vec<Value>,
        model: Option<&str>,
        effort: Option<&str>,
        sandbox_policy: Value,
    ) -> Result<String> {
        let response = self
            .request(
                "turn/start",
                turn_start_params(thread_id, input, model, effort, sandbox_policy),
            )
            .await?;
        response
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| anyhow!("Codex app-server did not return a turn id"))
    }

    pub(crate) async fn interrupt(&self, thread_id: &str, turn_id: &str) -> Result<()> {
        self.request(
            "turn/interrupt",
            json!({ "threadId": thread_id, "turnId": turn_id }),
        )
        .await?;
        Ok(())
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.notifications.subscribe()
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let (tx, rx) = oneshot::channel();
        self.pending.borrow_mut().insert(id, tx);
        if self
            .writer
            .send(json!({ "id": id, "method": method, "params": params }))
            .is_err()
        {
            self.pending.borrow_mut().remove(&id);
            bail!("Codex app-server is not running");
        }
        tokio::time::timeout(REQUEST_TIMEOUT, rx)
            .await
            .map_err(|_| anyhow!("Codex app-server request timed out: {method}"))?
            .map_err(|_| anyhow!("Codex app-server dropped response: {method}"))?
            .map_err(anyhow::Error::msg)
    }

    fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.writer
            .send(json!({ "method": method, "params": params }))
            .map_err(|_| anyhow!("Codex app-server is not running"))
    }
}

fn turn_start_params(
    thread_id: &str,
    input: Vec<Value>,
    model: Option<&str>,
    effort: Option<&str>,
    sandbox_policy: Value,
) -> Value {
    let mut params = json!({
        "threadId": thread_id,
        "input": input,
        "approvalPolicy": "never",
        "sandboxPolicy": sandbox_policy,
    });
    if let Some(model) = model {
        params["model"] = json!(model);
    }
    if let Some(effort) = effort {
        params["effort"] = Value::String(effort.to_owned());
    }
    params
}

/// Workspace-write sandbox policy scoped to `cwd`. Mirrors the Codex CLI's
/// default sandbox: write only inside the project, no network.
pub(crate) fn workspace_write_sandbox(cwd: &Path) -> Value {
    json!({
        "type": "workspaceWrite",
        "writableRoots": [cwd],
        "networkAccess": false,
        "excludeTmpdirEnvVar": false,
        "excludeSlashTmp": false
    })
}

/// Full-host sandbox policy for `--full-access`. Lets Codex run commands and
/// touch files anywhere on the host; callers must gate this behind an
/// explicit user flag.
pub(crate) fn danger_full_access_sandbox() -> Value {
    json!({ "type": "dangerFullAccess" })
}

fn is_server_request(message: &Value) -> bool {
    message.get("id").is_some() && message.get("method").is_some()
}

fn answer_server_request(writer: &mpsc::UnboundedSender<Value>, request: &Value) {
    let Some(id) = request.get("id").cloned() else {
        return;
    };
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let response = match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            json!({ "id": id, "result": { "decision": "decline" } })
        }
        _ => json!({
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("Grok Build does not yet support Codex server request: {method}")
            }
        }),
    };
    let _ = writer.send(response);
}

fn format_rpc_error(error: &Value) -> String {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Codex app-server request failed");
    match error.get("code").and_then(Value::as_i64) {
        Some(code) => format!("{message} ({code})"),
        None => message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_rpc_errors_with_codes() {
        assert_eq!(
            format_rpc_error(&json!({ "code": -32000, "message": "no thread" })),
            "no thread (-32000)"
        );
    }

    #[test]
    fn turn_start_forwards_explicit_reasoning_effort() {
        let params = turn_start_params(
            "thread-1",
            vec![json!({ "type": "text", "text": "hello" })],
            Some("gpt-5.6-sol"),
            Some("ultra"),
            workspace_write_sandbox(Path::new("/tmp/project")),
        );
        assert_eq!(params["effort"], "ultra");
        assert_eq!(params["model"], "gpt-5.6-sol");
        assert_eq!(params["sandboxPolicy"]["type"], "workspaceWrite");
    }

    #[test]
    fn turn_start_omits_model_when_none() {
        let params = turn_start_params(
            "thread-1",
            vec![json!({ "type": "text", "text": "hello" })],
            None,
            None,
            danger_full_access_sandbox(),
        );
        assert!(params.get("model").is_none());
        assert_eq!(params["sandboxPolicy"]["type"], "dangerFullAccess");
    }
}
