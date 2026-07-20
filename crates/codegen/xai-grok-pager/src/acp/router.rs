//! Provider router presented to the pager as one ACP agent.

use std::{cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc, sync::Arc};

use agent_client_protocol as acp;
use serde_json::{Value, json};
use xai_acp_lib::AcpGatewaySender;
use xai_grok_shell::agent::MvpAgent;

use crate::codex_app_server::{CodexAppServer, CodexModel, CodexStatus};

pub(crate) const CODEX_MODEL_PREFIX: &str = "codex:";

pub(crate) fn is_codex_model(model_id: &acp::ModelId) -> bool {
    model_id.0.as_ref().starts_with(CODEX_MODEL_PREFIX)
}

#[derive(Clone)]
struct CodexBackend {
    server: Rc<CodexAppServer>,
    status: CodexStatus,
}

#[derive(Debug, Clone)]
enum ActiveProvider {
    Grok,
    Codex {
        model: String,
        effort: Option<String>,
    },
}

#[derive(Debug, Clone)]
struct SessionRoute {
    cwd: PathBuf,
    active: ActiveProvider,
    codex_thread_id: Option<String>,
    active_codex_turn_id: Option<String>,
}

pub(crate) struct ProviderRouterAgent {
    grok: Rc<MvpAgent>,
    client: AcpGatewaySender<acp::AgentSide>,
    codex: RefCell<Option<CodexBackend>>,
    codex_unavailable: RefCell<Option<String>>,
    sessions: RefCell<HashMap<String, SessionRoute>>,
}

impl ProviderRouterAgent {
    pub(crate) fn new(grok: Rc<MvpAgent>, client: AcpGatewaySender<acp::AgentSide>) -> Self {
        Self {
            grok,
            client,
            codex: RefCell::new(None),
            codex_unavailable: RefCell::new(None),
            sessions: RefCell::new(HashMap::new()),
        }
    }

    async fn discover_codex(&self) {
        if self.codex.borrow().is_some() || self.codex_unavailable.borrow().is_some() {
            return;
        }
        let binary = std::env::var_os("GROK_CODEX_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("codex"));
        let result = async {
            let server = Rc::new(CodexAppServer::start(&binary).await?);
            let status = server.status().await?;
            anyhow::Ok(CodexBackend { server, status })
        }
        .await;
        match result {
            Ok(backend) => {
                tracing::info!(
                    account = %backend.status.account_label,
                    models = backend.status.models.len(),
                    "Codex subscription connected"
                );
                *self.codex.borrow_mut() = Some(backend);
            }
            Err(error) => {
                tracing::info!(error = %error, "Codex subscription unavailable");
                *self.codex_unavailable.borrow_mut() = Some(error.to_string());
            }
        }
    }

    fn codex_backend(&self) -> Result<CodexBackend, acp::Error> {
        self.codex.borrow().clone().ok_or_else(|| {
            let detail = self
                .codex_unavailable
                .borrow()
                .clone()
                .unwrap_or_else(|| "Codex was not discovered".to_owned());
            acp::Error::internal_error().data(format!(
                "Codex subscription is unavailable: {detail}. Run `codex login`, then restart Grok Build."
            ))
        })
    }

    fn merge_model_state(&self, state: &mut acp::SessionModelState) {
        let Some(backend) = self.codex.borrow().as_ref().cloned() else {
            return;
        };
        for model in &backend.status.models {
            let info = codex_model_info(model, &backend.status.account_label);
            if !state
                .available_models
                .iter()
                .any(|existing| existing.model_id == info.model_id)
            {
                state.available_models.push(info);
            }
        }
    }

    fn merge_initialize_models(&self, response: &mut acp::InitializeResponse) {
        let Some(meta) = response.meta.as_mut() else {
            return;
        };
        let Some(value) = meta.get("modelState").cloned() else {
            return;
        };
        let Ok(mut state) = serde_json::from_value::<acp::SessionModelState>(value) else {
            return;
        };
        self.merge_model_state(&mut state);
        if let Ok(value) = serde_json::to_value(state) {
            meta.insert("modelState".to_owned(), value);
        }
        meta.insert(
            "grokProviders".to_owned(),
            json!({
                "grok": { "connected": true },
                "codex": self.codex.borrow().as_ref().map(|backend| json!({
                    "connected": true,
                    "account": backend.status.account_label
                })).unwrap_or_else(|| json!({
                    "connected": false,
                    "error": self.codex_unavailable.borrow().clone()
                }))
            }),
        );
    }

    fn merge_response_models(&self, response: &mut acp::NewSessionResponse) {
        if let Some(state) = response.models.as_mut() {
            self.merge_model_state(state);
        }
    }

    fn merge_load_models(&self, response: &mut acp::LoadSessionResponse) {
        if let Some(state) = response.models.as_mut() {
            self.merge_model_state(state);
        }
    }

    async fn prompt_codex(
        &self,
        args: acp::PromptRequest,
        model: String,
        effort: Option<String>,
    ) -> Result<acp::PromptResponse, acp::Error> {
        let backend = self.codex_backend()?;
        let session_key = args.session_id.0.to_string();
        let (cwd, existing_thread) = self
            .sessions
            .borrow()
            .get(&session_key)
            .map(|route| (route.cwd.clone(), route.codex_thread_id.clone()))
            .ok_or_else(|| acp::Error::invalid_params().data("unknown session route"))?;

        let thread_id = match existing_thread {
            Some(thread_id) => thread_id,
            None => {
                let thread_id = backend
                    .server
                    .start_thread(&cwd, &model)
                    .await
                    .map_err(codex_error)?;
                if let Some(route) = self.sessions.borrow_mut().get_mut(&session_key) {
                    route.codex_thread_id = Some(thread_id.clone());
                }
                thread_id
            }
        };
        let input = codex_input(&args.prompt)?;
        let mut notifications = backend.server.subscribe();
        let turn_id = backend
            .server
            .start_turn(&thread_id, input, &model, effort.as_deref(), &cwd)
            .await
            .map_err(codex_error)?;
        if let Some(route) = self.sessions.borrow_mut().get_mut(&session_key) {
            route.active_codex_turn_id = Some(turn_id.clone());
        }
        let prompt_meta = args.meta.clone();

        loop {
            let notification = match notifications.recv().await {
                Ok(notification) => notification,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "lagged behind Codex app-server notifications");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    self.clear_active_turn(&session_key);
                    return Err(codex_error(anyhow::anyhow!(
                        "Codex app-server notification stream closed"
                    )));
                }
            };
            if notification
                .pointer("/params/threadId")
                .and_then(Value::as_str)
                .is_some_and(|id| id != thread_id)
            {
                continue;
            }
            let method = notification.get("method").and_then(Value::as_str);
            match method {
                Some("item/agentMessage/delta") => {
                    if let Some(delta) = notification
                        .pointer("/params/delta")
                        .and_then(Value::as_str)
                    {
                        self.send_update(
                            args.session_id.clone(),
                            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                                acp::ContentBlock::Text(acp::TextContent::new(delta)),
                            )),
                            prompt_meta.clone(),
                        )
                        .await?;
                    }
                }
                Some(
                    "item/reasoning/summaryTextDelta"
                    | "item/reasoning/textDelta"
                    | "item/plan/delta",
                ) => {
                    if let Some(delta) = notification
                        .pointer("/params/delta")
                        .and_then(Value::as_str)
                    {
                        self.send_update(
                            args.session_id.clone(),
                            acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(
                                acp::ContentBlock::Text(acp::TextContent::new(delta)),
                            )),
                            prompt_meta.clone(),
                        )
                        .await?;
                    }
                }
                Some("item/started") => {
                    if let Some(tool_call) = codex_tool_start(&notification) {
                        self.send_update(
                            args.session_id.clone(),
                            acp::SessionUpdate::ToolCall(tool_call),
                            prompt_meta.clone(),
                        )
                        .await?;
                    }
                }
                Some("item/completed") => {
                    if let Some(tool_update) = codex_tool_complete(&notification) {
                        self.send_update(
                            args.session_id.clone(),
                            acp::SessionUpdate::ToolCallUpdate(tool_update),
                            prompt_meta.clone(),
                        )
                        .await?;
                    }
                }
                Some("error")
                    if notification.pointer("/params/willRetry") != Some(&Value::Bool(true)) =>
                {
                    let message = notification
                        .pointer("/params/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("Codex app-server reported an error");
                    self.clear_active_turn(&session_key);
                    return Err(acp::Error::internal_error().data(message.to_owned()));
                }
                Some("turn/completed") => {
                    if notification
                        .pointer("/params/turn/id")
                        .and_then(Value::as_str)
                        != Some(turn_id.as_str())
                    {
                        continue;
                    }
                    self.clear_active_turn(&session_key);
                    let status = notification
                        .pointer("/params/turn/status")
                        .and_then(Value::as_str)
                        .unwrap_or("completed");
                    return match status {
                        "failed" => Err(acp::Error::internal_error().data(
                            notification
                                .pointer("/params/turn/error/message")
                                .and_then(Value::as_str)
                                .unwrap_or("Codex turn failed")
                                .to_owned(),
                        )),
                        "interrupted" | "cancelled" => {
                            Ok(acp::PromptResponse::new(acp::StopReason::Cancelled))
                        }
                        _ => Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)),
                    };
                }
                _ => {}
            }
        }
    }

    fn clear_active_turn(&self, session_key: &str) {
        if let Some(route) = self.sessions.borrow_mut().get_mut(session_key) {
            route.active_codex_turn_id = None;
        }
    }

    async fn send_update(
        &self,
        session_id: acp::SessionId,
        update: acp::SessionUpdate,
        meta: Option<acp::Meta>,
    ) -> Result<(), acp::Error> {
        acp::Client::session_notification(
            &self.client,
            acp::SessionNotification::new(session_id, update).meta(meta),
        )
        .await
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for ProviderRouterAgent {
    async fn initialize(
        &self,
        args: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        let mut response = self.grok.initialize(args).await?;
        self.discover_codex().await;
        self.merge_initialize_models(&mut response);
        Ok(response)
    }

    async fn authenticate(
        &self,
        args: acp::AuthenticateRequest,
    ) -> Result<acp::AuthenticateResponse, acp::Error> {
        self.grok.authenticate(args).await
    }

    async fn new_session(
        &self,
        args: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        let cwd = args.cwd.clone();
        let mut response = self.grok.new_session(args).await?;
        self.sessions.borrow_mut().insert(
            response.session_id.0.to_string(),
            SessionRoute {
                cwd,
                active: ActiveProvider::Grok,
                codex_thread_id: None,
                active_codex_turn_id: None,
            },
        );
        self.merge_response_models(&mut response);
        Ok(response)
    }

    async fn prompt(&self, args: acp::PromptRequest) -> Result<acp::PromptResponse, acp::Error> {
        let provider = self
            .sessions
            .borrow()
            .get(args.session_id.0.as_ref())
            .map(|route| route.active.clone())
            .unwrap_or(ActiveProvider::Grok);
        match provider {
            ActiveProvider::Grok => self.grok.prompt(args).await,
            ActiveProvider::Codex { model, effort } => self.prompt_codex(args, model, effort).await,
        }
    }

    async fn cancel(&self, args: acp::CancelNotification) -> Result<(), acp::Error> {
        let active_codex = self
            .sessions
            .borrow()
            .get(args.session_id.0.as_ref())
            .and_then(|route| {
                Some((
                    route.codex_thread_id.clone()?,
                    route.active_codex_turn_id.clone()?,
                ))
            });
        if let Some((thread_id, turn_id)) = active_codex {
            return self
                .codex_backend()?
                .server
                .interrupt(&thread_id, &turn_id)
                .await
                .map_err(codex_error);
        }
        self.grok.cancel(args).await
    }

    async fn load_session(
        &self,
        args: acp::LoadSessionRequest,
    ) -> Result<acp::LoadSessionResponse, acp::Error> {
        let cwd = args.cwd.clone();
        let session_id = args.session_id.clone();
        let mut response = self.grok.load_session(args).await?;
        self.sessions.borrow_mut().insert(
            session_id.0.to_string(),
            SessionRoute {
                cwd,
                active: ActiveProvider::Grok,
                codex_thread_id: None,
                active_codex_turn_id: None,
            },
        );
        self.merge_load_models(&mut response);
        Ok(response)
    }

    async fn set_session_mode(
        &self,
        args: acp::SetSessionModeRequest,
    ) -> Result<acp::SetSessionModeResponse, acp::Error> {
        self.grok.set_session_mode(args).await
    }

    async fn set_session_model(
        &self,
        args: acp::SetSessionModelRequest,
    ) -> Result<acp::SetSessionModelResponse, acp::Error> {
        let requested = args.model_id.0.as_ref();
        if let Some(model) = requested.strip_prefix(CODEX_MODEL_PREFIX) {
            let backend = self.codex_backend()?;
            let model_info = backend
                .status
                .models
                .iter()
                .find(|entry| entry.id == model)
                .ok_or_else(|| {
                    acp::Error::invalid_params()
                        .data(format!("unknown Codex subscription model: {model}"))
                })?;
            let requested_effort = args
                .meta
                .as_ref()
                .and_then(|meta| meta.get("reasoningEffort"))
                .and_then(Value::as_str);
            let effort = resolve_codex_effort(model_info, requested_effort)?;
            let mut sessions = self.sessions.borrow_mut();
            let route = sessions
                .get_mut(args.session_id.0.as_ref())
                .ok_or_else(|| acp::Error::invalid_params().data("unknown session route"))?;
            if route.active_codex_turn_id.is_some() {
                return Err(acp::Error::invalid_params()
                    .data("cancel or finish the active Codex turn before switching models"));
            }
            route.active = ActiveProvider::Codex {
                model: model.to_owned(),
                effort: effort.clone(),
            };
            return Ok(acp::SetSessionModelResponse::new().meta(
                json!({
                    "provider": "codex",
                    "modelId": format!("{CODEX_MODEL_PREFIX}{model}"),
                    "reasoningEffort": effort,
                    "context": "Codex and Grok keep separate conversation branches in this session"
                })
                .as_object()
                .cloned(),
            ));
        }

        let response = self.grok.set_session_model(args.clone()).await?;
        if let Some(route) = self
            .sessions
            .borrow_mut()
            .get_mut(args.session_id.0.as_ref())
        {
            route.active = ActiveProvider::Grok;
        }
        Ok(response)
    }

    async fn set_session_config_option(
        &self,
        args: acp::SetSessionConfigOptionRequest,
    ) -> Result<acp::SetSessionConfigOptionResponse, acp::Error> {
        self.grok.set_session_config_option(args).await
    }

    async fn list_sessions(
        &self,
        args: acp::ListSessionsRequest,
    ) -> Result<acp::ListSessionsResponse, acp::Error> {
        self.grok.list_sessions(args).await
    }

    async fn ext_method(&self, args: acp::ExtRequest) -> Result<acp::ExtResponse, acp::Error> {
        self.grok.ext_method(args).await
    }

    async fn ext_notification(&self, args: acp::ExtNotification) -> Result<(), acp::Error> {
        self.grok.ext_notification(args).await
    }
}

fn codex_model_info(model: &CodexModel, account_label: &str) -> acp::ModelInfo {
    let id = acp::ModelId::new(Arc::<str>::from(format!(
        "{CODEX_MODEL_PREFIX}{}",
        model.id
    )));
    let description = model.description.as_deref().map_or_else(
        || format!("OpenAI Codex via {account_label}"),
        |description| format!("{description} - OpenAI Codex via {account_label}"),
    );
    let reasoning_efforts: Vec<Value> = model
        .supported_reasoning_efforts
        .iter()
        .map(|effort| {
            json!({
                "id": effort.value,
                "value": effort.value,
                "label": codex_effort_label(&effort.value),
                "description": effort.description,
                "default": model.default_reasoning_effort.as_deref() == Some(effort.value.as_str()),
            })
        })
        .collect();
    let mut meta = json!({
        "provider": "codex",
        "providerModelId": model.id,
        "acceptsImages": false,
        "isProviderDefault": model.is_default,
        "supportsReasoningEffort": !model.supported_reasoning_efforts.is_empty(),
        "reasoningEfforts": reasoning_efforts,
        "reasoningEffort": model.default_reasoning_effort,
    });
    if let Some(object) = meta.as_object_mut() {
        object.retain(|_, value| !value.is_null());
    }
    acp::ModelInfo::new(id, format!("Codex - {}", model.name))
        .description(description)
        .meta(meta.as_object().cloned())
}

fn codex_effort_label(value: &str) -> &str {
    match value {
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra High",
        "max" => "Max",
        "ultra" => "Ultra",
        _ => value,
    }
}

fn resolve_codex_effort(
    model: &CodexModel,
    requested: Option<&str>,
) -> Result<Option<String>, acp::Error> {
    let Some(requested) = requested else {
        return Ok(model.default_reasoning_effort.clone());
    };
    if model
        .supported_reasoning_efforts
        .iter()
        .any(|effort| effort.value == requested)
    {
        return Ok(Some(requested.to_owned()));
    }
    let offered = model
        .supported_reasoning_efforts
        .iter()
        .map(|effort| effort.value.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Err(acp::Error::invalid_params().data(format!(
        "Codex model {} does not support reasoning effort '{requested}'; use one of: {offered}",
        model.name
    )))
}

fn codex_input(blocks: &[acp::ContentBlock]) -> Result<Vec<Value>, acp::Error> {
    let mut input = Vec::new();
    for block in blocks {
        match block {
            acp::ContentBlock::Text(text) => {
                input.push(json!({ "type": "text", "text": text.text }));
            }
            acp::ContentBlock::ResourceLink(resource) => {
                input.push(json!({
                    "type": "text",
                    "text": format!("Referenced resource: {}", resource.uri)
                }));
            }
            acp::ContentBlock::Resource(resource) => {
                input.push(json!({
                    "type": "text",
                    "text": serde_json::to_string(resource).unwrap_or_default()
                }));
            }
            acp::ContentBlock::Image(_) | acp::ContentBlock::Audio(_) => {
                return Err(acp::Error::invalid_params()
                    .data("the Codex connector currently supports text prompts only"));
            }
            _ => {
                return Err(acp::Error::invalid_params().data("unsupported prompt block for Codex"));
            }
        }
    }
    if input.is_empty() {
        return Err(acp::Error::invalid_params().data("Codex prompt is empty"));
    }
    Ok(input)
}

fn codex_tool_start(notification: &Value) -> Option<acp::ToolCall> {
    let item = notification.pointer("/params/item")?;
    let id = item.get("id")?.as_str()?;
    let item_type = item.get("type")?.as_str()?;
    let (title, kind) = match item_type {
        "commandExecution" => (
            format!(
                "Shell: {}",
                item.get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("command")
            ),
            acp::ToolKind::Execute,
        ),
        "fileChange" => ("Apply file changes".to_owned(), acp::ToolKind::Edit),
        "webSearch" => ("Web search".to_owned(), acp::ToolKind::Search),
        "mcpToolCall" => (
            format!(
                "{}.{}",
                item.get("server").and_then(Value::as_str).unwrap_or("mcp"),
                item.get("tool").and_then(Value::as_str).unwrap_or("tool")
            ),
            acp::ToolKind::Other,
        ),
        "collabAgentToolCall" => ("Collaboration agent".to_owned(), acp::ToolKind::Other),
        _ => return None,
    };
    Some(
        acp::ToolCall::new(acp::ToolCallId::new(Arc::<str>::from(id)), title)
            .kind(kind)
            .status(acp::ToolCallStatus::InProgress)
            .raw_input(item.clone()),
    )
}

fn codex_tool_complete(notification: &Value) -> Option<acp::ToolCallUpdate> {
    let item = notification.pointer("/params/item")?;
    let id = item.get("id")?.as_str()?;
    codex_tool_kind(item.get("type")?.as_str()?)?;
    let failed = item
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| matches!(status, "failed" | "declined"));
    Some(acp::ToolCallUpdate::new(
        acp::ToolCallId::new(Arc::<str>::from(id)),
        acp::ToolCallUpdateFields::new()
            .status(if failed {
                acp::ToolCallStatus::Failed
            } else {
                acp::ToolCallStatus::Completed
            })
            .raw_output(item.clone()),
    ))
}

fn codex_tool_kind(item_type: &str) -> Option<acp::ToolKind> {
    match item_type {
        "commandExecution" => Some(acp::ToolKind::Execute),
        "fileChange" => Some(acp::ToolKind::Edit),
        "webSearch" => Some(acp::ToolKind::Search),
        "mcpToolCall" | "collabAgentToolCall" => Some(acp::ToolKind::Other),
        _ => None,
    }
}

fn codex_error(error: anyhow::Error) -> acp::Error {
    acp::Error::internal_error().data(format!("Codex: {error:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_app_server::CodexReasoningEffort;

    #[test]
    fn codex_models_have_collision_safe_ids_and_provider_labels() {
        let info = codex_model_info(
            &CodexModel {
                id: "gpt-5.4".to_owned(),
                name: "GPT-5.4".to_owned(),
                description: None,
                is_default: true,
                supported_reasoning_efforts: vec![CodexReasoningEffort {
                    value: "medium".to_owned(),
                    description: Some("Balanced reasoning".to_owned()),
                }],
                default_reasoning_effort: Some("medium".to_owned()),
            },
            "ChatGPT plus subscription",
        );
        assert_eq!(info.model_id.0.as_ref(), "codex:gpt-5.4");
        assert!(is_codex_model(&info.model_id));
        assert_eq!(info.name, "Codex - GPT-5.4");
        assert_eq!(
            info.meta.as_ref().and_then(|meta| meta.get("provider")),
            Some(&json!("codex"))
        );
        let meta = info.meta.unwrap();
        assert_eq!(meta.get("supportsReasoningEffort"), Some(&json!(true)));
        assert_eq!(meta.get("reasoningEffort"), Some(&json!("medium")));
        assert_eq!(
            meta.get("reasoningEfforts")
                .and_then(Value::as_array)
                .and_then(|options| options.first())
                .and_then(|option| option.get("description")),
            Some(&json!("Balanced reasoning"))
        );
    }

    #[test]
    fn codex_effort_resolution_uses_default_and_accepts_ultra() {
        let model = CodexModel {
            id: "gpt-5.6-sol".to_owned(),
            name: "GPT-5.6-Sol".to_owned(),
            description: None,
            is_default: true,
            supported_reasoning_efforts: vec![
                CodexReasoningEffort {
                    value: "low".to_owned(),
                    description: None,
                },
                CodexReasoningEffort {
                    value: "max".to_owned(),
                    description: Some("Maximum single-agent depth".to_owned()),
                },
                CodexReasoningEffort {
                    value: "ultra".to_owned(),
                    description: Some("Automatic task delegation".to_owned()),
                },
            ],
            default_reasoning_effort: Some("low".to_owned()),
        };
        assert_eq!(
            resolve_codex_effort(&model, None).unwrap().as_deref(),
            Some("low")
        );
        assert_eq!(
            resolve_codex_effort(&model, Some("ultra"))
                .unwrap()
                .as_deref(),
            Some("ultra")
        );
        assert!(resolve_codex_effort(&model, Some("medium")).is_err());

        let info = codex_model_info(&model, "ChatGPT subscription");
        let options =
            xai_grok_shell::sampling::types::parse_reasoning_efforts_meta(info.meta.as_ref())
                .unwrap();
        assert_eq!(
            options
                .iter()
                .map(|option| (option.label.as_str(), option.value))
                .collect::<Vec<_>>(),
            vec![
                ("Low", xai_grok_shell::sampling::types::ReasoningEffort::Low),
                ("Max", xai_grok_shell::sampling::types::ReasoningEffort::Max),
                (
                    "Ultra",
                    xai_grok_shell::sampling::types::ReasoningEffort::Ultra,
                ),
            ]
        );
    }

    #[test]
    fn tool_events_keep_codex_item_ids() {
        let notification = json!({
            "params": { "item": { "id": "item-7", "type": "commandExecution", "command": "pwd" } }
        });
        let tool = codex_tool_start(&notification).unwrap();
        assert_eq!(tool.tool_call_id.0.as_ref(), "item-7");
        assert_eq!(tool.status, acp::ToolCallStatus::InProgress);
    }
}
